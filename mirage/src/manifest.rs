//! Manifest settings, candidate discovery, and merge policies.
//!
//! [`Manifest`] describes the template root, configure candidates, and optional Luau module.
//! [`ManifestCandidate::resolve`] expands candidate paths and globs. [`Candidate::combine`]
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

use crate::filesystem::ExpandedPath;

/// The format used for configure candidates in the daemon.
pub type DefaultFormat = Toml;

/// The default filename extension used to discover template candidates.
///
/// Template discovery and event filtering use [`ManifestConfigure::template_extension`] when set.
pub const DEFAULT_TEMPLATE_EXTENSION: &str = "jinja";

/// The default quiet interval in milliseconds before rendering after an input change.
///
/// Each relevant input event restarts the wait. See [`ManifestConfigure::notificate_period`].
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
    /// Replace conflicting scalar values and arrays through [`Figment::merge`].
    #[default]
    Override,

    /// Replace conflicting scalars and concatenate arrays through [`Figment::admerge`].
    Append,

    /// Keep existing values and fill missing keys through [`Figment::join`].
    Fallback,

    /// Keep existing values and concatenate arrays through [`Figment::adjoin`].
    Supplement,
}

/// A path pattern and merge policy declared by a `[[candidate]]` entry.
#[derive(Debug, Serialize, Deserialize)]
pub struct ManifestCandidate {
    /// The candidate path.
    ///
    /// Shell expansion and glob matching occur in [`Self::resolve`].
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
    pub fn resolve(
        &self,
        relative_root: impl AsRef<Path>,
    ) -> eyre::Result<impl FusedIterator<Item = Candidate>> {
        let relative_root = relative_root.as_ref();

        let &Self { ref path, policy } = self;

        let mut target_list = Vec::new();

        // NOTE: Absolute paths produced by expansion supply their own root for glob matching.
        let expanded_path = ExpandedPath::new(path)?.path();

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
            MergePolicy::Append => Figment::admerge(target_value, target_data),
            MergePolicy::Fallback => Figment::join(target_value, target_data),
            MergePolicy::Supplement => Figment::adjoin(target_value, target_data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Candidate, DefaultFormat, MergePolicy};

    use figment::{Figment, providers::Data};

    #[test]
    fn policies() {
        let directory = tempfile::tempdir().unwrap();

        let base = directory.path().join("base.toml");
        let overlay = directory.path().join("overlay.toml");

        std::fs::write(&base, "[default]\nscalar = 'base'\narray = ['base']\n[default.nested]\nretained = 1\nconflict = 'base'\n").unwrap();
        std::fs::write(&overlay, "[default]\nscalar = 'candidate'\narray = ['candidate']\nadded = 'value'\n[default.nested]\nintroduced = 2\nconflict = 'candidate'\n").unwrap();

        for (policy, scalar, array) in [
            (MergePolicy::Override, "candidate", vec!["candidate"]),
            (MergePolicy::Append, "candidate", vec!["base", "candidate"]),
            (MergePolicy::Fallback, "base", vec!["base"]),
            (MergePolicy::Supplement, "base", vec!["base", "candidate"]),
        ] {
            let path = overlay.clone();

            let candidate = Candidate { path, policy };

            let base = Figment::new().merge(Data::<DefaultFormat>::file(&base).nested());

            let result = candidate
                .combine::<DefaultFormat>(base)
                .extract::<toml::Value>()
                .unwrap();

            let expected = toml::Value::try_from(array).unwrap();

            assert_eq!(result["scalar"].as_str(), Some(scalar), "{policy:?}");
            assert_eq!(result["array"], expected, "{policy:?}");
            assert_eq!(result["added"].as_str(), Some("value"), "{policy:?}");
            assert_eq!(
                result["nested"]["retained"].as_integer(),
                Some(1),
                "{policy:?}"
            );
            assert_eq!(
                result["nested"]["introduced"].as_integer(),
                Some(2),
                "{policy:?}"
            );
            assert_eq!(
                result["nested"]["conflict"].as_str(),
                Some(scalar),
                "{policy:?}"
            );
        }
    }
}
