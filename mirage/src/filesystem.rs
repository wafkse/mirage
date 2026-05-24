//! Filesystem-related utilities.

use std::ops::Deref;
use std::ops::DerefMut;

use notify::Watcher;
use notify::{Event, EventHandler, RecommendedWatcher};

use tokio::sync::mpsc;

/// A filesystem watcher, encompassing `notify::RecommendedWatcher`.
#[derive(Debug)]
pub struct FsWatcher(mpsc::UnboundedReceiver<Event>, RecommendedWatcher);

impl FsWatcher {
    /// Instantiate the standard configuration for this filesystem watcher.
    ///
    /// This is identical to `FsWatcher::configured(Config::default())`.
    #[inline]
    pub fn standard() -> anyhow::Result<Self> {
        Self::configured(notify::Config::default())
    }

    /// Instantiate a filesystem watcher with the provided configuration.
    #[inline]
    pub fn configured(target_configure: notify::Config) -> anyhow::Result<Self> {
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

/// A filesystem watch event forwarded, provided to a `notify::RecommendedWatcher` as the [`EventHandler`].
///
/// This sends to the other end of the pipe to a [`FsWatcher`].
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
