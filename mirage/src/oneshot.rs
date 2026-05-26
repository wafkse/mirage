//! One-shot asynchronous execution.

/// A trait for asynchronous oneshot [`Future`]s.
pub trait Oneshot {
    /// The output of the [`Future`].
    type Output;

    /// Perform an oneshot activation.
    fn oneshot(&mut self) -> impl Future<Output = eyre::Result<Self::Output>>;
}
