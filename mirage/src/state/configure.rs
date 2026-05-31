//! Configure state management.

use std::ops::Deref;

use figment::{Figment, Profile, providers::Env};

use notify::{RecursiveMode, Watcher};

use tokio::sync::watch;

use crate::{
    background::Background,
    filesystem::FsWatcher,
    manifest::{self, Candidate, MergePolicy},
};

/// The configure pipe of a [`ConfigureState`].
#[derive(Debug, Clone)]
pub struct ConfigurePipe(pub watch::Sender<tera::Context>, pub watch::Sender<Profile>);

impl ConfigurePipe {
    /// Determine the send half for the Tera context.
    #[inline]
    pub const fn context(&self) -> &watch::Sender<tera::Context> {
        let Self(target_value, ..) = self;

        target_value
    }

    /// Determine the send half for the Tera context, mutably.
    #[inline]
    pub const fn context_mut(&mut self) -> &mut watch::Sender<tera::Context> {
        let Self(target_value, ..) = self;

        target_value
    }

    /// Determine the send half for the selected profile.
    #[inline]
    pub const fn profile(&self) -> &watch::Sender<Profile> {
        let Self(.., target_value) = self;

        target_value
    }

    /// Determine the send half for the selected profile, mutably.
    #[inline]
    pub const fn profile_mut(&mut self) -> &mut watch::Sender<Profile> {
        let Self(.., target_value) = self;

        target_value
    }
}

/// A configure candidate state.
///
/// This encompasses a [`FsWatcher`] and a [`ConfigureCandidate`] list, keeping the watchlist and each configure candidate in sync.
#[derive(Debug)]
pub struct ConfigureState {
    /// The configure candidate filesystem watcher.
    configure_watch: FsWatcher,

    /// The list of configure candidates.
    candidate_list: Vec<Candidate>,

    /// The pipe used for Tera context and profile management.
    configure_pipe: ConfigurePipe,
}

impl ConfigureState {
    /// Instantiate a [`ConfigureState`] with the target configure candidate list.
    #[inline]
    pub fn new(candidate_list: Vec<Candidate>, target_profile: Profile) -> eyre::Result<Self> {
        let configure_watch = {
            let mut configure_watch = FsWatcher::standard()?;

            for Candidate { path, .. } in candidate_list.as_slice() {
                configure_watch.watch(path, RecursiveMode::NonRecursive)?;
            }

            configure_watch
        };

        let target_context = candidate_list
            .iter()
            .fold(Figment::new(), |target_value: Figment, target_candidate| {
                Candidate::combine::<manifest::DefaultFormat>(target_candidate, target_value)
            })
            .merge(Env::prefixed("MIRAGE_"))
            .select(target_profile.clone())
            .extract::<toml::Value>()
            .map(tera::Context::from_serialize)??;

        let (context_sender, ..) = watch::channel(target_context);

        let (profile_sender, ..) = watch::channel(target_profile);

        let configure_pipe = ConfigurePipe(context_sender, profile_sender);

        Ok(Self {
            configure_watch,
            candidate_list,
            configure_pipe,
        })
    }

    /// Determine the watched pipe pair.
    #[inline]
    pub const fn pipe(&self) -> &ConfigurePipe {
        let Self { configure_pipe, .. } = self;

        configure_pipe
    }

    /// Determine the watched pipe pair, mutably.
    #[inline]
    pub const fn pipe_mut(&mut self) -> &mut ConfigurePipe {
        let Self { configure_pipe, .. } = self;

        configure_pipe
    }
}

impl Background for ConfigureState {
    type Output = ();

    async fn run(self) -> eyre::Result<Self::Output> {
        let Self {
            mut configure_watch,
            mut candidate_list,
            configure_pipe: ConfigurePipe(context_send, profile_send),
            ..
        } = self;

        let mut profile_recv = profile_send.subscribe();

        /// The distinct events managed by the `ConfigureState`.
        #[derive(Debug)]
        enum ConfigureEvent {
            Filesystem(notify::Event),
            Profile,
        }

        loop {
            let target_value = tokio::select!(
                Some(target_event) = configure_watch.receiver_mut().recv() => ConfigureEvent::Filesystem(target_event),
                Ok(..) = profile_recv.changed() => ConfigureEvent::Profile,
            );

            let target_state = match target_value {
                ConfigureEvent::Filesystem(notify::Event {
                    kind: notify::EventKind::Create(notify::event::CreateKind::File),
                    paths: path_list,
                    ..
                }) => {
                    let mut target_change = false;

                    for path in path_list {
                        configure_watch.watch(path.as_path(), RecursiveMode::NonRecursive)?;

                        let policy = MergePolicy::default();

                        candidate_list.push(Candidate { path, policy });

                        target_change = true;
                    }

                    target_change
                }
                ConfigureEvent::Filesystem(notify::Event {
                    kind: notify::EventKind::Remove(notify::event::RemoveKind::File),
                    paths: path_list,
                    ..
                }) => {
                    let mut target_change = false;

                    for ref target_path in path_list {
                        // NOTE: There is no need to remove the removed file from the filesystem watcher as the `notify` crate will do it automatically.

                        candidate_list.retain(|Candidate { path, .. }| path != target_path);

                        target_change = true;
                    }

                    target_change
                }
                // NOTE: Unconditiionally re-hydrate on *any* modification event.
                ConfigureEvent::Filesystem(notify::Event {
                    kind: notify::EventKind::Modify(notify::event::ModifyKind::Data(..)),
                    ..
                }) => true,
                ConfigureEvent::Profile => true,
                _ => false,
            };

            if target_state {
                context_send.send(
                    candidate_list
                        .iter()
                        .fold(Figment::new(), |target_value, target_candidate| {
                            Candidate::combine::<manifest::DefaultFormat>(
                                target_candidate,
                                target_value,
                            )
                        })
                        .merge(Env::prefixed("MIRAGE_"))
                        .select(Profile::clone(profile_recv.borrow().deref()))
                        .extract::<toml::Value>()
                        .map(tera::Context::from_serialize)??,
                )?;
            }
        }
    }
}
