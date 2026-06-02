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

    /// Merge transient values into the current render context.
    ///
    /// Configure context rebuilding replaces these values.
    Mutate {
        /// Values to merge into the render context.
        context: serde_json::Value,
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
    async fn handle(
        ConfigurePipe(context_pipe, profile_pipe): &mut ConfigurePipe,
        target_request: ControlRequest,
    ) -> eyre::Result<ControlResponse> {
        match target_request {
            ControlRequest::Ping => Ok::<_, eyre::Error>(ControlResponse::Pong),
            ControlRequest::Profile { name } => {
                profile_pipe.send(name)?;

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Hydrate => {
                // NOTE: `send_modify` notifies subscribers while retaining the current context.
                context_pipe.send_modify(|_| ());

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Mutate { context } => {
                let override_context =
                    tera::Context::from_value(context).map_err(eyre::Report::from)?;

                context_pipe.send_modify(|target_context| target_context.extend(override_context));

                Ok(ControlResponse::Acknowledge)
            }
            ControlRequest::Context => {
                let context = context_pipe.borrow().clone().into_json();

                Ok(ControlResponse::Context { context })
            }
        }
    }
}

impl Background for ControlState {
    type Output = ();

    async fn run(mut self) -> eyre::Result<Self::Output> {
        let Self(ref mut target_endpoint, ref mut target_pipe) = self;

        let (ref state_send, mut state_recv) = mpsc::unbounded_channel::<(
            ControlRequest,
            oneshot::Sender<eyre::Result<ControlResponse>>,
        )>();

        enum Operation {
            Socket(UnixStream),
            Request(
                (
                    ControlRequest,
                    oneshot::Sender<eyre::Result<ControlResponse>>,
                ),
            ),
        }

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

                            let ref target_response = match target_response {
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
                    let _ = target_channel.send(
                        ControlState::handle(target_pipe, target_message)
                            .await
                            .map_err(eyre::Report::from),
                    );
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
