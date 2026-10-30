//! Asynchronous execution contract shared by the daemon subsystems.
//!
//! See the [`Background`] trait for additional information.

/// A subsystem that owns its state for the duration of an asynchronous task.
pub trait Background {
    /// The value produced when the subsystem finishes.
    type Output;

    /// Run the subsystem until completion or a terminal error.
    fn run(self) -> impl Future<Output = eyre::Result<Self::Output>> + Send + Sync;
}
