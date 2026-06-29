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

use crate::filesystem;

/// The format used for configure candidates in the daemon.
pub type DefaultFormat = Toml;

/// The default filename extension used to discover template candidates.
///
/// Template discovery and event filtering use [`ManifestConfigure::template_extension`] when set.
pub const DEFAULT_TEMPLATE_EXTENSION: &str = "jinja";

/// The default quiet interval in milliseconds before rendering after an input change.
///
/// Each event received during the interval restarts the wait. See [`ManifestConfigure::notificate_period`].
pub const DEFAULT_NOTIFICATE_PERIOD: u64 = 50;

/// The manifest of a Mirage-managed directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// The top-level manifest `[configure]` key.
    pub configure: ManifestConfigure,

    /// The candidate list specified in the manifest.
    pub candidate: Vec<ManifestCandidate>,

    /// The optional `[module]` section controlling access to host libraries.
    #[serde(default)]
    pub module: ManifestModule,
}

/// The top-level manifest `[configure]` key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub struct ManifestConfigure {
    /// The template root path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<PathBuf>,

    /// The path to the daemon control socket.
    #[serde(rename = "listen-sock", skip_serializing_if = "Option::is_none")]
    pub listen_sock: Option<PathBuf>,

    /// The profile to use by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,

    /// The path to the Luau module supplying template functions, filters, and globals.
    ///
    /// Shell expansion precedes module resolution relative to the configure root.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<PathBuf>,

    /// The template filename extension, defaulting to [`DEFAULT_TEMPLATE_EXTENSION`].
    #[serde(rename = "template-extension", skip_serializing_if = "Option::is_none")]
    pub template_extension: Option<String>,

    /// The undefined-reference behaviour for the rendering engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undefined: Option<UndefinedKind>,

    /// The quiet interval in milliseconds used to coalesce input events before rendering.
    ///
    /// Defaults to [`DEFAULT_NOTIFICATE_PERIOD`]. A value of `0` renders each relevant event immediately.
    #[serde(rename = "notificate-period", skip_serializing_if = "Option::is_none")]
    pub notificate_period: Option<u64>,
}

/// The handling of undefined references during a render.
///
/// Selects [`minijinja::UndefinedBehavior`]. The default is [`Self::Strict`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UndefinedKind {
    /// Printing, iterating, or testing an undefined value produces an error.
    #[default]
    Strict,

    /// Undefined values print as empty strings and iterate as empty sequences. Nested lookups fail.
    Lenient,

    /// Undefined values support nested lookups and print as empty strings.
    Chainable,
}

/// Access settings for Mirage host libraries used by Luau modules.
///
/// An empty or absent section yields an empty host-library allowlist. The Luau standard library
/// and file-based `require` remain available.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestModule {
    /// The allowlist of Mirage modules resolvable as `@mirage/<lib>` from module code.
    #[serde(default)]
    pub libraries: Vec<String>,
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
    /// Expand the path pattern and collect matching candidates relative to the supplied root.
    ///
    /// Absolute expanded paths supply their own root. Individual glob traversal errors are skipped.
    ///
    /// # Errors
    ///
    /// Returns an error when the candidate path cannot be expanded or its glob pattern is invalid.
    #[inline]
    pub fn resolve_at(
        &self,
        relative_root: impl AsRef<Path>,
    ) -> eyre::Result<impl FusedIterator<Item = Candidate>> {
        let relative_root = relative_root.as_ref();

        let &Self { ref path, policy } = self;

        let mut target_list = Vec::new();

        // NOTE: Absolute paths produced by expansion supply their own root for glob matching.
        let expanded_path = filesystem::shell_expand_str(path)?;

        for target_value in glob(
            relative_root
                .join(expanded_path)
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
    #[must_use]
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
