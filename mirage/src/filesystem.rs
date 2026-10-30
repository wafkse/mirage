//! Path expansion and filesystem change notifications.
//!
//! See [`ExpandedPath`] and [`FsWatcher`] for additional information.

use std::ops::Deref;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};

use eyre::WrapErr;

use notify::Watcher;
use notify::{Event, EventHandler, RecommendedWatcher};

use tokio::sync::mpsc;

/// A daemon path after shell-style expansion of its input expression.
///
/// Supports a leading `~` and environment references written as `$VAR` or `${VAR}`.
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
    pub fn path(self) -> PathBuf {
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
    /// Create a watcher using [`notify::Config::default`].
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

        let target_handler = FsEventForwarder(channel_send);
        let target_watcher = RecommendedWatcher::new(target_handler, target_configure)?;

        Ok(Self(channel_recv, target_watcher))
    }
}

impl FsWatcher {
    /// Receive filesystem change notifications.
    #[inline]
    #[must_use]
    pub const fn receiver(&self) -> &mpsc::UnboundedReceiver<Event> {
        let Self(target_recv, ..) = self;

        target_recv
    }

    /// Receive filesystem change notifications through a mutable receiver.
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
/// Events arriving after receiver shutdown and watcher errors are discarded.
// NOTE(invariant): None.
#[derive(Debug)]
struct FsEventForwarder(mpsc::UnboundedSender<Event>);

impl EventHandler for FsEventForwarder {
    #[inline]
    fn handle_event(&mut self, target_event: notify::Result<Event>) {
        if let Ok(target_event) = target_event {
            let Self(channel_send) = self;

            let _ = channel_send.send(target_event);
        }
    }
}
