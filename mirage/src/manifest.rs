//! Manifest settings, candidate discovery, and merge policies.
//!
//! [`Manifest`] describes the template root, configure candidates, and optional Luau module.
//! [`ManifestCandidate::resolve_at`] expands candidate paths and globs. [`Candidate::combine`]
//! applies each discovered file to the profile data using its declared [`MergePolicy`].

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
/// Each variant selects a [`Figment`] operation. Tables merge recursively in every policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MergePolicy {
    /// Replace conflicting values with candidate values through [`Figment::merge`].
    #[default]
    Override,

    /// Keep existing values and fill missing keys through [`Figment::join`].
    Append,

    /// Replace conflicting values and concatenate arrays through [`Figment::admerge`].
    Fallback,

    /// Keep existing values and concatenate arrays through [`Figment::adjoin`].
    Supplement,
}

/// A path pattern and merge policy declared by a `[[candidate]]` entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestCandidate {
    /// The candidate path.
    ///
    /// Shell expansion and glob matching occur in [`Self::resolve_at`].
    pub path: String,

    /// The merge policy for this candidate.
    pub policy: MergePolicy,
}

impl ManifestCandidate {
    /// Collect matching candidates relative to the supplied root.
    #[inline]
    pub fn resolve_at(
        self,
        relative_root: impl AsRef<Path>,
    ) -> eyre::Result<impl Iterator<Item = Candidate> + FusedIterator> {
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
    pub fn resolve(self) -> eyre::Result<impl Iterator<Item = Candidate> + FusedIterator> {
        Self::resolve_at(self, std::env::current_dir()?)
    }
}

/// A discovered configure file paired with its merge policy.
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

        let target_data = Data::<F>::nested(Data::<F>::file(path));

        match policy {
            MergePolicy::Override => Figment::merge(target_value, target_data),
            MergePolicy::Append => Figment::join(target_value, target_data),
            MergePolicy::Fallback => Figment::admerge(target_value, target_data),
            MergePolicy::Supplement => Figment::adjoin(target_value, target_data),
        }
    }
}
