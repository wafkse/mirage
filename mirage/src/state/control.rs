//! Control message types and socket handling for daemon requests.

use std::ops::{Deref, DerefMut};

use bytes::BytesMut;
use serde::{Deserialize, Serialize};
use tokio::net::{UnixDatagram, unix::SocketAddr};

use crate::oneshot::Oneshot;

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
}

/// Socket state for receiving and responding to daemon control requests.
// NOTE(invariant): None.
#[derive(Debug)]
pub struct ControlState {
    /// The control socket for this control state.
    control_socket: UnixDatagram,
}

impl ControlState {
    /// Instantiate a control state.
    #[inline]
    pub const fn new(control_socket: UnixDatagram) -> Self {
        Self { control_socket }
    }
}

impl Oneshot for ControlState {
    type Output = (SocketAddr, ControlRequest);

    async fn oneshot(&mut self) -> eyre::Result<Self::Output> {
        let Self { control_socket, .. } = self;

        let mut control_buffer = BytesMut::new();

        let (.., target_address) = control_socket.recv_buf_from(&mut control_buffer).await?;

        Ok((
            target_address,
            serde_json::from_slice::<ControlRequest>(&control_buffer[..])?,
        ))
    }
}

impl Deref for ControlState {
    type Target = UnixDatagram;

    fn deref(&self) -> &Self::Target {
        let Self { control_socket, .. } = self;

        control_socket
    }
}

impl DerefMut for ControlState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let Self { control_socket, .. } = self;

        control_socket
    }
}
