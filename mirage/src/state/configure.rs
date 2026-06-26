//! Configure state management.

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

/// Fold the configure candidate tree into a single render context under the selected profile.
///
/// Each manifest candidate is expanded against the candidate root, the resulting files are combined under their merge
/// policies, the `MIRAGE_`-prefixed environment overlays are merged, and the chosen profile is selected out. Both the
/// daemon's reactive `ConfigureState` and the synchronous one-shot render funnel through this so the two paths can never
/// drift apart.
///
/// # Errors
///
/// Returns an error when a candidate path cannot be expanded or globbed, or when the folded tree fails to deserialize.
pub(crate) fn fold_context(
    candidate_tree: &Path,
    manifest_candidates: &[ManifestCandidate],
    context_profile: &Profile,
) -> eyre::Result<toml::Value> {
    let mut configure_list = Vec::new();

    for target_value in manifest_candidates
        .iter()
        .map(|target_candidate| target_candidate.resolve_at(candidate_tree))
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

/// The configure pipe of a [`ConfigureState`].
#[derive(Debug, Clone)]
pub struct ConfigurePipe(pub watch::Sender<toml::Value>, pub watch::Sender<Profile>);

impl ConfigurePipe {
    /// Determine the send half for the render context.
    #[inline]
    #[must_use]
    pub const fn context(&self) -> &watch::Sender<toml::Value> {
        let Self(target_value, ..) = self;

        target_value
    }

    /// Determine the send half for the render context, mutably.
    #[inline]
    pub const fn context_mut(&mut self) -> &mut watch::Sender<toml::Value> {
        let Self(target_value, ..) = self;

        target_value
    }

    /// Determine the send half for the selected profile.
    #[inline]
    #[must_use]
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

    /// The root to tree of configure candidates.
    candidate_tree: PathBuf,

    /// The list of manifest candidates.
    ///
    /// This is used to categorise and filter the existing candidates in the configure directory.
    manifest_candidates: Vec<ManifestCandidate>,

    /// The pipe used for Tera context and profile management.
    configure_pipe: ConfigurePipe,
}

impl ConfigureState {
    /// Instantiate a [`ConfigureState`] with the target configure candidate list.
    ///
    /// # Errors
    ///
    /// Returns an error when the candidate tree cannot be watched or the initial context fails to fold.
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

        let target_context =
            fold_context(candidate_tree, &manifest_candidates, &context_profile)?;

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

    /// Determine the watched pipe pair.
    #[inline]
    #[must_use]
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
            manifest_candidates,
            configure_pipe: ConfigurePipe(context_send, profile_send),
            candidate_tree,
            ..
        } = self;

        let ref candidate_tree = if candidate_tree.is_relative() {
            std::env::current_dir()?.join(candidate_tree)
        } else {
            candidate_tree
        };

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
                _ => false,
            };

            if target_state {
                let configure_list = {
                    let mut target_list = Vec::new();

                    for target_value in manifest_candidates
                        .iter()
                        .map(|target_candidate| target_candidate.resolve_at(candidate_tree))
                    {
                        let target_value = target_value?;

                        target_list.extend(target_value);
                    }

                    target_list
                };

                let target_context = configure_list
                    .iter()
                    .fold(Figment::new(), |target_value: Figment, target_candidate| {
                        target_candidate.combine::<manifest::DefaultFormat>(target_value)
                    })
                    .merge(Env::prefixed("MIRAGE_"))
                    .select(Profile::clone(profile_recv.borrow().deref()))
                    .extract::<toml::Value>()
                    .map(tera::Context::from_serialize)??;

                context_send.send(target_context)?;
            }
        }
    }
}
