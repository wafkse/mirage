//! Control state management.

use std::net::Shutdown;

use tokio::net::{UnixDatagram, unix::SocketAddr};

use bytes::{Buf, BytesMut};

use serde::{Deserialize, Serialize};

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
    /// This forces override of the provided context value-tree on top of the already-established render context.
    ///
    /// This is ephemeral and does not persist across re-hydrate operations.
    Mutate {
        /// The to mutate the source context with.
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

/// The control state.
#[derive(Debug)]
pub struct ControlState(UnixDatagram, ConfigurePipe);

impl ControlState {
    /// Instantiate a control state.
    #[inline]
    pub const fn new(control_socket: UnixDatagram, configure_pipe: ConfigurePipe) -> Self {
        Self(control_socket, configure_pipe)
    }

    /// Handle a control message.
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
                // NOTE: This fakes out a mutation, which triggers a re-hydrate without us having to re-create the context.
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

        loop {
            let mut recv_buf = BytesMut::new();

            let (byte_count, target_address) = target_endpoint.recv_buf_from(&mut recv_buf).await?;

            let target_response =
                match serde_json::from_slice::<ControlRequest>(&recv_buf[..byte_count])
                    .map_err(eyre::Report::from)
                {
                    Ok(target_message) => ControlState::handle(target_pipe, target_message).await,
                    Err(target_error) => Err(target_error),
                };

            recv_buf.advance(usize::MIN);

            let target_response = match target_response
                .map(|ref target_value| serde_json::to_string(target_value))
                .map(|target_value| target_value.map_err(eyre::Report::from))
            {
                Ok(Ok(target_response)) => target_response,
                Ok(Err(target_error)) | Err(target_error) => {
                    serde_json::to_string(&ControlResponse::Error {
                        message: target_error.to_string(),
                    })
                    .expect("failed to serialize error message")
                }
            };

            if let Some(target_pathname) = target_address.as_pathname() {
                let target_response = target_response.as_bytes();

                let mut byte_count = target_response.len();

                while byte_count > 0 {
                    byte_count -= target_endpoint
                        .send_to(target_response, target_pathname)
                        .await?;
                }
            };
        }
    }
}

impl Drop for ControlState {
    fn drop(&mut self) {
        let Self(target_endpoint, ..) = self;

        let _ = target_endpoint.shutdown(Shutdown::Both);

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
