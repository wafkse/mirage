//! Runtime control requests over a Unix domain socket.
//!
//! Connections exchange length-prefixed JSON messages. [`ControlState`] serializes request
//! handling against the configure channels while connection tasks perform socket I/O.

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
    /// Deep-merge a TOML fragment into the current render context and notify the hydration worker.
    ///
    /// Mutations remain until the configure subsystem rebuilds the context from files, environment
    /// values, and the selected profile. A [`Self::Hydrate`] request retains the mutated context.
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
    /// The context update or profile notification was accepted. Rendering proceeds asynchronously.
    Acknowledge,

    /// A context message.
    ///
    /// Contains the current render context encoded as JSON.
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

/// A control listener and the channels used to update daemon state.
///
/// Incoming connections forward requests to a serial dispatch loop. Dropping the state removes
/// the listener's socket path when it can be determined.
// NOTE(invariant): None.
#[derive(Debug)]
pub struct ControlState(UnixListener, ConfigurePipe);

impl ControlState {
    /// Instantiate a control state.
    #[inline]
    pub const fn new(control_listener: UnixListener, configure_pipe: ConfigurePipe) -> Self {
        Self(control_listener, configure_pipe)
    }

    /// Apply a request to the configure channels and construct its response.
    ///
    /// Profile and context updates notify their subscribers before acknowledgement. Subscribers
    /// perform context rebuilding and template rendering independently.
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

                // NOTE: `send_modify` notifies subscribers while retaining the current context.
                context_pipe.send_modify(|_| ());

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Mutate { expression } => {
                tracing::info!(%expression, "mutating context");

                // NOTE: `toml_edit` accepts dotted-key fragments and preserves TOML value kinds.
                let override_context = toml_edit::de::from_str::<toml::Value>(expression.as_str())
                    .map_err(eyre::Report::from)?;

                context_pipe.send_modify(|target_context| {
                    Self::deep_merge(target_context, override_context);
                });

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Context => {
                tracing::debug!("dumping context");

                let context = serde_json::to_value(context_pipe.borrow().clone())?;

                Ok(ControlResponse::Context { context })
            }
        }
    }

    /// Deep-merge an overlay value tree onto a base render context.
    ///
    /// Tables merge recursively. Arrays, scalars, and values of different kinds are replaced by the overlay.
    fn deep_merge(target_context: &mut toml::Value, override_context: toml::Value) {
        match (target_context, override_context) {
            (toml::Value::Table(target_table), toml::Value::Table(override_table)) => {
                for (target_key, override_value) in override_table {
                    match target_table.get_mut(&target_key) {
                        Some(target_value) => Self::deep_merge(target_value, override_value),
                        None => {
                            let _ = target_table.insert(target_key, override_value);
                        }
                    }
                }
            }
            (target_context, override_context) => *target_context = override_context,
        }
    }
}

impl Background for ControlState {
    type Output = ();

    async fn run(mut self) -> eyre::Result<Self::Output> {
        /// Work serialized by the control dispatch loop.
        enum Operation {
            /// A newly accepted client connection.
            Socket(UnixStream),

            /// A decoded request paired with its response channel.
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
    use super::ControlState;

    #[test]
    fn tables_merge_while_arrays_and_scalars_replace() {
        let mut target_context = toml::from_str::<toml::Value>(
            "[system]\nbattery = \"full\"\nload = 1\ntags = [\"a\"]\n",
        )
        .unwrap();

        let override_context =
            toml::from_str::<toml::Value>("[system]\nbattery = \"low\"\ntags = [\"b\", \"c\"]\n")
                .unwrap();

        ControlState::deep_merge(&mut target_context, override_context);

        let target_system = target_context.get("system").unwrap();

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

        ControlState::deep_merge(&mut target_context, override_context);

        let target_system = target_context.get("system").unwrap();

        assert_eq!(target_system.get("battery").unwrap().as_str(), Some("full"));
        assert_eq!(
            target_system.get("battery-state").unwrap().as_str(),
            Some("low")
        );
    }
}
