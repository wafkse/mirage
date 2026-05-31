//! Background-runnable subsystems in Mirage.

/// A trait for asynchronous oneshot [`Future`]s.
pub trait Background {
    /// The output of the [`Future`].
    type Output;

    /// Perform an oneshot activation.
    fn run(self) -> impl Future<Output = eyre::Result<Self::Output>> + Send + Sync;
}
