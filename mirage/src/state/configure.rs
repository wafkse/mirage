//! Configuration inputs and profile selection.
//!
//! See [`ConfigureState`] and [`ConfigurePipe`] for additional information.

use std::{
    ops::Deref,
    path::{Path, PathBuf},
};

use figment::{Figment, Profile, providers::Env};

use glob::Pattern;
use notify::{RecursiveMode, Watcher};

use tokio::sync::watch;

use crate::{
    background::Background,
    filesystem::FsWatcher,
    manifest::{self, ManifestCandidate},
};

/// Senders for the render context and selected profile of a [`ConfigureState`].
#[derive(Debug, Clone)]
pub struct ConfigurePipe(pub watch::Sender<toml::Value>, pub watch::Sender<Profile>);

impl ConfigurePipe {
    /// Access the sender for render-context updates.
    #[inline]
    #[must_use]
    pub const fn context(&self) -> &watch::Sender<toml::Value> {
        let Self(target_value, ..) = self;

        target_value
    }

    /// Mutably access the sender for render-context updates.
    #[inline]
    pub const fn context_mut(&mut self) -> &mut watch::Sender<toml::Value> {
        let Self(target_value, ..) = self;

        target_value
    }

    /// Access the sender for profile changes.
    #[inline]
    #[must_use]
    pub const fn profile(&self) -> &watch::Sender<Profile> {
        let Self(.., target_value) = self;

        target_value
    }

    /// Mutably access the sender for profile changes.
    #[inline]
    pub const fn profile_mut(&mut self) -> &mut watch::Sender<Profile> {
        let Self(.., target_value) = self;

        target_value
    }
}

/// Watch configure inputs and publish the selected profile's resolved context.
///
/// File and profile changes replace the current context, including any transient mutations.
/// Candidate patterns are matched again so newly added files can contribute values.
// NOTE(invariant): None.
#[derive(Debug)]
pub struct ConfigureState {
    /// The configure candidate filesystem watcher.
    configure_watch: FsWatcher,

    /// The root directory used to resolve candidate patterns.
    candidate_tree: PathBuf,

    /// The list of manifest candidates.
    ///
    /// These patterns select relevant filesystem events and files to load during a rebuild.
    manifest_candidates: Vec<ManifestCandidate>,

    /// The channels used for render context and profile updates.
    configure_pipe: ConfigurePipe,
}

impl ConfigureState {
    /// Instantiate a [`ConfigureState`] with the target configure candidate list.
    ///
    /// # Errors
    ///
    /// Returns an error when the candidate tree cannot be watched or the initial configuration cannot be loaded.
    #[inline]
    pub fn new(
        (candidate_tree, manifest_candidates): (&Path, Vec<ManifestCandidate>),
        context_profile: Profile,
    ) -> eyre::Result<Self> {
        let configure_watch = {
            let mut target_value = FsWatcher::standard()?;

            target_value.watch(candidate_tree, RecursiveMode::Recursive)?;

            target_value
        };

        let target_context = Self::fold(candidate_tree, &manifest_candidates, &context_profile)?;

        let (context_sender, ..) = watch::channel(target_context);

        let (profile_sender, ..) = watch::channel(context_profile);

        let configure_pipe = ConfigurePipe(context_sender, profile_sender);

        let candidate_tree = candidate_tree.to_path_buf();

        Ok(Self {
            configure_watch,
            candidate_tree,
            manifest_candidates,
            configure_pipe,
        })
    }

    /// Access the context and profile update channels.
    #[inline]
    #[must_use]
    pub const fn pipe(&self) -> &ConfigurePipe {
        let Self { configure_pipe, .. } = self;

        configure_pipe
    }

    /// Mutably access the context and profile update channels.
    #[inline]
    pub const fn pipe_mut(&mut self) -> &mut ConfigurePipe {
        let Self { configure_pipe, .. } = self;

        configure_pipe
    }

    /// Combine configure candidates and select the active profile's render context.
    ///
    /// Expand candidate patterns in manifest order, apply their merge policies, and overlay `MIRAGE_`
    /// environment values.
    ///
    /// # Errors
    ///
    /// Returns an error when a candidate path cannot be expanded or globbed, or when the folded tree fails to deserialize.
    pub(crate) fn fold(
        candidate_tree: &Path,
        manifest_candidates: &[ManifestCandidate],
        context_profile: &Profile,
    ) -> eyre::Result<toml::Value> {
        let mut configure_list = Vec::new();

        for target_value in manifest_candidates
            .iter()
            .map(|target_candidate| target_candidate.resolve(candidate_tree))
        {
            configure_list.extend(target_value?);
        }

        tracing::debug!(
            candidate_count = configure_list.len(),
            profile = %context_profile,
            "folding configure candidates",
        );

        let target_context = configure_list
            .iter()
            .fold(Figment::new(), |target_value: Figment, target_candidate| {
                target_candidate.combine::<manifest::DefaultFormat>(target_value)
            })
            .merge(Env::prefixed("MIRAGE_"))
            .select(context_profile.clone())
            .extract::<toml::Value>()?;

        Ok(target_context)
    }
}

impl Background for ConfigureState {
    type Output = ();

    #[inline]
    async fn run(self) -> eyre::Result<Self::Output> {
        /// Input changes handled by [`ConfigureState`].
        #[derive(Debug)]
        enum ConfigureEvent {
            /// An event from the configure tree watcher.
            Filesystem(notify::Event),

            /// The selected profile changed.
            Profile,
        }

        let Self {
            mut configure_watch,
            manifest_candidates,
            configure_pipe: ConfigurePipe(context_send, profile_send),
            candidate_tree,
            ..
        } = self;

        let candidate_tree = &if candidate_tree.is_relative() {
            std::env::current_dir()?.join(candidate_tree)
        } else {
            candidate_tree
        };

        let mut profile_recv = profile_send.subscribe();

        loop {
            let target_value = tokio::select!(
                Some(target_event) = configure_watch.receiver_mut().recv() => ConfigureEvent::Filesystem(target_event),
                Ok(..) = profile_recv.changed() => ConfigureEvent::Profile,
            );

            let target_state = match target_value {
                ConfigureEvent::Filesystem(notify::Event {
                    kind:
                        notify::EventKind::Create(notify::event::CreateKind::File)
                        | notify::EventKind::Remove(notify::event::RemoveKind::File)
                        | notify::EventKind::Modify(notify::event::ModifyKind::Data(..)),

                    paths: path_list,
                    ..
                }) => {
                    path_list.as_slice();

                    let mut target_change = false;

                    let pattern_list: Vec<Pattern> = manifest_candidates
                        .iter()
                        .map(|ManifestCandidate { path, .. }| path)
                        .map(String::as_str)
                        .map(Pattern::new)
                        .collect::<Result<_, glob::PatternError>>()?;

                    for file in path_list {
                        let file = file.strip_prefix(candidate_tree.as_path())?;

                        target_change = pattern_list
                            .iter()
                            .any(|pattern| pattern.matches_path(file));

                        if target_change {
                            break;
                        }
                    }

                    target_change
                }

                ConfigureEvent::Profile => true,
                ConfigureEvent::Filesystem(..) => false,
            };

            if target_state {
                let target_context = Self::fold(
                    candidate_tree.as_path(),
                    &manifest_candidates,
                    &Profile::clone(profile_recv.borrow().deref()),
                )?;

                tracing::info!("configure context recomputed");

                context_send.send(target_context)?;
            }
        }
    }
}
