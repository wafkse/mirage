//! Configure state management.

use std::ops::Deref;

use figment::{Figment, Profile, providers::Env};

use notify::{RecursiveMode, Watcher};

use tokio::sync::watch;

use crate::{
    filesystem::FsWatcher,
    manifest::{self, Candidate, MergePolicy},
    oneshot::Oneshot,
};

/// A configure candidate state.
///
/// This encompasses a [`FsWatcher`] and a [`ConfigureCandidate`] list, keeping the watchlist and each configure candidate in sync.
#[derive(Debug)]
pub struct ConfigureState {
    /// The configure candidate filesystem watcher.
    configure_watch: FsWatcher,

    /// The list of configure candidates.
    candidate_list: Vec<Candidate>,

    /// The pipe used for Tera context management.
    context_pipe: watch::Sender<tera::Context>,

    /// The pipe used for profile context management.
    profile_pipe: (watch::Sender<Profile>, watch::Receiver<Profile>),
}

impl ConfigureState {
    /// Instantiate a [`ConfigureState`] with the target configure candidate list.
    #[inline]
    pub fn new(candidate_list: Vec<Candidate>, target_profile: Profile) -> anyhow::Result<Self> {
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

        let (context_pipe, ..) = watch::channel(target_context);

        let profile_pipe = watch::channel(target_profile);

        Ok(Self {
            configure_watch,
            candidate_list,
            context_pipe,
            profile_pipe,
        })
    }

    /// Determine the watched Tera context.
    #[inline]
    pub const fn context(&self) -> &watch::Sender<tera::Context> {
        let Self { context_pipe, .. } = self;

        context_pipe
    }

    /// Determine the watched Profile.
    #[inline]
    pub const fn profile(&self) -> &watch::Sender<Profile> {
        let Self {
            profile_pipe: (profile_send, ..),
            ..
        } = self;

        profile_send
    }
}

impl Oneshot for ConfigureState {
    type Output = ();

    async fn oneshot(&mut self) -> anyhow::Result<Self::Output> {
        let Self {
            configure_watch,
            candidate_list,
            context_pipe,
            profile_pipe: (.., profile_recv),
        } = self;

        /// The distinct events managed by the `ConfigureState`.
        #[derive(Debug)]
        enum ConfigureEvent {
            Filesystem(notify::Event),
            Profile,
        }

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
            context_pipe.send(
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

        Ok(())
    }
}
