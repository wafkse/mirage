//! Control state management.

use std::ops::{Deref, DerefMut};

use bytes::BytesMut;
use serde::{Deserialize, Serialize};
use tokio::net::UnixDatagram;

use crate::oneshot::Oneshot;

/// A control message request.
#[derive(Debug, Serialize, Deserialize)]
pub enum ControlRequest {
    /// A ping message.
    Ping,

    /// A profile change message.
    Profile(figment::Profile),
}

/// A control message response.
#[derive(Debug, Serialize, Deserialize)]
pub enum ControlResponse {
    /// A pong message.
    Pong,

    /// An acknowledgement message.
    ///
    /// This is used for side-effectful control requests to indicate that it was sucessfully processed.
    Acknowledge,
}

/// The control state.
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
    type Output = ControlRequest;

    async fn oneshot(&mut self) -> anyhow::Result<Self::Output> {
        let Self { control_socket, .. } = self;

        let mut control_buffer = BytesMut::new();

        control_socket.recv_buf(&mut control_buffer).await?;

        Ok(serde_json::from_slice::<ControlRequest>(
            &control_buffer[..],
        )?)
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
