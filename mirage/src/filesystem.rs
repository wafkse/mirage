//! Path expansion and asynchronous delivery of filesystem events.
//!
//! [`FsWatcher`] retains a platform watcher and receives its successful events through a channel.

use std::ops::Deref;
use std::ops::DerefMut;
use std::path::{Path, PathBuf};

use eyre::WrapErr;

use notify::Watcher;
use notify::{Event, EventHandler, RecommendedWatcher};

use tokio::sync::mpsc;

/// Apply shell-style expansion to a path-bearing string.
///
/// Expand a leading `~` and `$VAR` or `${VAR}` environment references in a daemon path input.
///
/// # Errors
///
/// Returns an error when a referenced environment variable cannot be resolved.
#[inline]
pub fn shell_expand_str(target_path: &str) -> eyre::Result<String> {
    shellexpand::full(target_path)
        .map(std::borrow::Cow::into_owned)
        .wrap_err_with(|| format!("could not expand path `{target_path}`"))
}

/// Apply shell-style expansion to the lossy UTF-8 representation of a path.
///
/// This is the [`Path`]-typed counterpart of [`shell_expand_str`], used for the daemon's `-C`/`-T`/`-L` inputs.
///
/// # Errors
///
/// Returns an error when a referenced environment variable cannot be resolved.
#[inline]
pub fn shell_expand(target_path: impl AsRef<Path>) -> eyre::Result<PathBuf> {
    shell_expand_str(target_path.as_ref().to_string_lossy().as_ref()).map(PathBuf::from)
}

/// A platform watcher with a channel for successful filesystem events.
///
/// Watch registration is available through [`Deref`]. Events arrive through [`Self::receiver_mut`].
// NOTE(invariant): None.
#[derive(Debug)]
pub struct FsWatcher(mpsc::UnboundedReceiver<Event>, RecommendedWatcher);

impl FsWatcher {
    /// Construct a filesystem watcher using [`notify::Config::default`].
    #[inline]
    pub fn standard() -> eyre::Result<Self> {
        Self::configured(notify::Config::default())
    }

    /// Instantiate a filesystem watcher with the provided configuration.
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

            let _ = channel_send
                .send(target_event)
                .expect("failed to send filesystem event");
        }
    }
}
