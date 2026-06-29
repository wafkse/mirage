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

use crate::filesystem;

/// The format used for configure candidates in the daemon.
pub type DefaultFormat = Toml;

/// The default extension borne by template candidates.
///
/// The self-recursion guard and template loader both key on this. Overriding it via [`ManifestConfigure::template_extension`] requires renaming the template files to match (set it to `"tera"` to migrate a legacy tree without renaming).
pub const DEFAULT_TEMPLATE_EXTENSION: &str = "jinja";

/// The default grace period, in milliseconds, over which a burst of filesystem notifications is coalesced.
///
/// Filesystem watchers routinely emit several events for a single logical edit (a rename, then a data write, then a
/// metadata touch). Holding the render back for this window collapses such a burst into a single hydration pass. See
/// [`ManifestConfigure::notificate_period`].
pub const DEFAULT_NOTIFICATE_PERIOD: u64 = 50;

/// The manifest of a Mirage-managed directory.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// The top-level manifest `[configure]` key.
    pub configure: ManifestConfigure,

    /// The candidate list specified in the manifest.
    pub candidate: Vec<ManifestCandidate>,

    /// The optional `[module]` key gating the Luau scripting seam.
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

    /// The path to the Luau module supplying template function_table and filter_table.
    ///
    /// This is resolved relative to the configure root after shell expansion, mirroring `require` resolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub module: Option<PathBuf>,

    /// The extension borne by template candidates, defaulting to [`DEFAULT_TEMPLATE_EXTENSION`].
    #[serde(rename = "template-extension", skip_serializing_if = "Option::is_none")]
    pub template_extension: Option<String>,

    /// The undefined-reference behaviour for the rendering engine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undefined: Option<UndefinedKind>,

    /// The grace period, in milliseconds, over which a burst of filesystem notifications is coalesced before a render.
    ///
    /// This defaults to [`DEFAULT_NOTIFICATE_PERIOD`]; a value of `0` disables coalescing so every notification renders
    /// eagerly. The `rename_all = "lowercase"` rule leaves field names untouched, so the dashed wire key is spelt out.
    #[serde(rename = "notificate-period", skip_serializing_if = "Option::is_none")]
    pub notificate_period: Option<u64>,
}

/// The handling of undefined references during a render.
///
/// This maps onto `minijinja::UndefinedBehavior`, defaulting to the [`UndefinedKind::Strict`] variant so that a missing
/// reference becomes a hard render error and, by way of the all-or-nothing transaction, leaves existing outputs untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UndefinedKind {
    /// A missing reference is a hard error wherever it surfaces.
    #[default]
    Strict,

    /// A missing reference renders as nothing and is otherwise inert.
    Lenient,

    /// A missing reference may be chained into further lookups, erroring only when finally rendered.
    Chainable,
}

/// The optional `[module]` manifest key.
///
/// This declares the Mirage-provided modules exposed to module code on top of the vanilla Luau standard library. It is
/// intentionally permissive of absence: an empty or missing section yields the bare Luau standard library alone.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ManifestModule {
    /// The allowlist of Mirage modules resolvable as `@mirage/<lib>` from module code.
    #[serde(default)]
    pub libraries: Vec<String>,
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
    /// This may be a glob pattern that may need future normalization.
    pub path: String,

    /// The merge policy for this candidate.
    pub policy: MergePolicy,
}

impl ManifestCandidate {
    /// Resolve a manifest candidate to a bare candidate at a relative path root.
    ///
    /// To work with the Current Working Directory, use the [`ManifestCandidate::resolve`] associated function.
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

        // NOTE: Expand before the join/glob so that a literal `~` never survives into a path component and a
        // post-expansion absolute candidate replaces, rather than appends to, the relative root.
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
