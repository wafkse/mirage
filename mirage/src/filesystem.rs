//! Path expansion and asynchronous delivery of filesystem events.
//!
//! [`ExpandedPath`] resolves shell expressions in daemon path inputs.
//! [`FsWatcher`] retains a platform watcher and receives its successful events through a channel.

use std::ops::Deref;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};

use eyre::WrapErr;

use notify::Watcher;
use notify::{Event, EventHandler, RecommendedWatcher};

use tokio::sync::mpsc;

/// A daemon path after shell-style expansion of its input expression.
///
/// Construction expands a leading `~` and environment references through `shellexpand`.
// NOTE(invariant): None.
#[derive(Debug, Clone)]
pub struct ExpandedPath(PathBuf);

impl ExpandedPath {
    /// Expand a path expression after conversion to its lossy UTF-8 representation.
    ///
    /// # Errors
    ///
    /// Returns an error when a referenced environment variable cannot be resolved.
    #[inline]
    pub fn new(target_path: impl AsRef<Path>) -> eyre::Result<Self> {
        let target_path = target_path.as_ref().to_string_lossy();

        let target_value = shellexpand::full(&target_path)
            .map(std::borrow::Cow::into_owned)
            .wrap_err_with(|| format!("could not expand path `{target_path}`"))?;

        let target_value = PathBuf::from(target_value);

        Ok(Self(target_value))
    }

    /// Consume the expanded input and return its filesystem path.
    #[inline]
    #[must_use]
    pub fn into_path(self) -> PathBuf {
        let Self(target_path) = self;

        target_path
    }
}

/// A platform watcher with a channel for successful filesystem events.
///
/// Watch registration is available through [`Deref`]. Events arrive through [`Self::receiver_mut`].
// NOTE(invariant): None.
#[derive(Debug)]
pub struct FsWatcher(mpsc::UnboundedReceiver<Event>, RecommendedWatcher);

impl FsWatcher {
    /// Instantiate the standard configuration for this filesystem watcher.
    ///
    /// Uses [`notify::Config::default`].
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying platform watcher cannot be constructed.
    #[inline]
    pub fn standard() -> eyre::Result<Self> {
        Self::configured(notify::Config::default())
    }

    /// Instantiate a filesystem watcher with the provided configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying platform watcher cannot be constructed.
    #[inline]
    pub fn configured(target_configure: notify::Config) -> eyre::Result<Self> {
        let (channel_send, channel_recv) = mpsc::unbounded_channel();

        Ok(Self(
            channel_recv,
            RecommendedWatcher::new(FsEventForwarder(channel_send), target_configure)?,
        ))
    }
}

impl FsWatcher {
    /// Borrow the `mpsc` receiver attached for this filesystem watcher.
    #[inline]
    #[must_use]
    pub const fn receiver(&self) -> &mpsc::UnboundedReceiver<Event> {
        let Self(target_recv, ..) = self;

        target_recv
    }

    /// Mutably borrow the `mpsc` receiver attached for this filesystem watcher.
    #[inline]
    pub const fn receiver_mut(&mut self) -> &mut mpsc::UnboundedReceiver<Event> {
        let Self(target_recv, ..) = self;

        target_recv
    }
}

impl Deref for FsWatcher {
    type Target = RecommendedWatcher;

    #[inline]
    fn deref(&self) -> &Self::Target {
        let Self(.., target_watcher) = self;

        target_watcher
    }
}

impl DerefMut for FsWatcher {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        let Self(.., target_watcher) = self;

        target_watcher
    }
}

/// Forward successful watcher events to the receiving [`FsWatcher`].
///
/// Sending an event panics if the receiver has been dropped. Watcher errors are discarded.
// NOTE(invariant): None.
#[derive(Debug)]
struct FsEventForwarder(mpsc::UnboundedSender<Event>);

impl EventHandler for FsEventForwarder {
    #[inline]
    fn handle_event(&mut self, target_event: notify::Result<Event>) {
        if let Ok(target_event) = target_event {
            let Self(channel_send) = self;

            channel_send
                .send(target_event)
                .expect("failed to send filesystem event");
        }
    }
}
