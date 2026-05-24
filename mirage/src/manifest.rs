//! Mirage manifest structure.

use serde::{Deserialize, Serialize};
use std::{
    iter::FusedIterator,
    path::{Path, PathBuf},
};

use figment::{
    Figment,
    providers::{Data, Format, Toml},
};

use glob::glob;

/// The format used for configure candidates in the daemon.
pub type DefaultFormat = Toml;

/// The manifest of a Mirage-managed directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// The top-level manifest `[configure]` key.
    pub configure: ManifestConfigure,

    /// The candidate list specified in the manifest.
    pub candidate: Vec<ManifestCandidate>,
}

/// The top-level manifest `[configure]` key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct ManifestConfigure {
    /// The template root path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<PathBuf>,

    /// The path to the daemon control socket.
    pub listen_sock: Option<PathBuf>,

    /// The profile to use by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// A merge policy for individual configure candidates.
///
/// This specifies the *Conflict Resolution* strategy used for each respective
/// candidate when fused with the primary [`Figment`] state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MergePolicy {
    /// The override merge policy. Uses [`Figment::merge`].
    #[default]
    Override,

    /// The append merge policy. Uses [`Figment::join`].
    Append,

    /// The fallback merge policy. Uses [`Figment::admerge`].
    Fallback,

    /// The supplement merge policy. Uses [`Figment::adjoin`].
    Supplement,
}

/// A manifest candidate.
#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestCandidate {
    /// The candidate path.
    ///
    /// This may be a glob pattern.
    pub path: String,

    /// The merge policy for this candidate.
    pub policy: MergePolicy,
}

impl ManifestCandidate {
    /// Resolve a manifest candidate to a bare candidate at a relative path root.
    ///
    /// To work with the Current Working Directory, use the [`ManifestCandidate::resolve`] associated function.
    #[inline]
    pub fn resolve_at(
        self,
        relative_root: impl AsRef<Path>,
    ) -> anyhow::Result<impl Iterator<Item = Candidate> + FusedIterator> {
        let relative_root = relative_root.as_ref();

        let Self { path, policy } = self;

        let mut target_list = Vec::new();

        for target_value in glob(
            relative_root
                .join(path)
                .to_string_lossy()
                .to_string()
                .as_str(),
        )? {
            let Ok(path) = target_value else {
                continue;
            };

            target_list.push(Candidate { path, policy });
        }

        Ok(target_list.into_iter())
    }

    /// Resolve a manifest candidate to a bare candidate relative the working directory.
    #[inline]
    pub fn resolve(self) -> anyhow::Result<impl Iterator<Item = Candidate> + FusedIterator> {
        Self::resolve_at(self, std::env::current_dir()?)
    }
}

/// A singular candidate for template parameter provision.
#[derive(Debug)]
pub struct Candidate {
    /// The candidate path.
    pub path: PathBuf,

    /// The merge policy for this candidate.
    pub policy: MergePolicy,
}

impl Candidate {
    /// Apply the target merge policy to the provided file for the [`Figment`] state.
    #[inline]
    pub fn combine<F>(&self, target_value: Figment) -> Figment
    where
        F: Format,
    {
        let Self { path, policy } = self;

        let provider = Data::<F>::file(path).nested();

        match policy {
            MergePolicy::Override => Figment::merge(target_value, provider),
            MergePolicy::Append => Figment::join(target_value, provider),
            MergePolicy::Fallback => Figment::admerge(target_value, provider),
            MergePolicy::Supplement => Figment::adjoin(target_value, provider),
        }
    }
}
