//! Hydrate state management.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use notify::{RecursiveMode, Watcher};
use tempfile::tempdir_in;
use tokio::{
    fs::{self},
    sync::watch,
};

use tera::Tera;

use crate::{filesystem::FsWatcher, oneshot::Oneshot};

/// The hydration state for a set of configure candidates.
#[derive(Debug)]
pub struct HydrationState {
    /// The Tera state.
    tera_state: Tera,

    /// The filesystem watcher for *template* files.
    watcher_template: FsWatcher,

    /// The pipe used for Tera context management.
    context_pipe: watch::Receiver<tera::Context>,

    /// The render path root with the hydrated templates.
    template_root: PathBuf,

    /// A boolean that indicates whether the startup hydrate has been performed.
    start_hydrate: bool,
}

impl HydrationState {
    /// Instantiate a new hydration state.
    #[inline]
    pub fn new(
        template_root: impl AsRef<Path>,
        context_pipe: watch::Receiver<tera::Context>,
    ) -> anyhow::Result<Self> {
        let tera_state = Tera::new(
            template_root
                .as_ref()
                .join("**/*.tera")
                .to_str()
                .ok_or_else(|| anyhow!("template root is bad utf-8"))?,
        )?;

        let mut watcher_template = FsWatcher::standard()?;

        watcher_template.watch(template_root.as_ref(), RecursiveMode::Recursive)?;

        let template_root = template_root.as_ref().to_path_buf();

        let start_hydrate = false;

        Ok(Self {
            tera_state,
            watcher_template,
            context_pipe,
            template_root,
            start_hydrate,
        })
    }
}

impl Oneshot for HydrationState {
    type Output = ();

    async fn oneshot(&mut self) -> anyhow::Result<Self::Output> {
        let Self {
            tera_state,
            watcher_template,
            context_pipe,
            template_root,
            start_hydrate,
        } = self;

        /// An event to the hydration subsystem.
        #[derive(Debug)]
        enum HydrationEvent {
            /// A filesystem event pertaining to the watched templates.
            Filesystem(notify::Event),

            /// The context was updated.
            Context,
        }

        let target_value = tokio::select!(
            Some(target_event) = watcher_template.receiver_mut().recv() => HydrationEvent::Filesystem(target_event),
            Ok(..) = context_pipe.changed() => HydrationEvent::Context
        );

        let target_change = if !*start_hydrate {
            *start_hydrate = true;

            true
        } else {
            match target_value {
                HydrationEvent::Filesystem(notify::Event {
                    kind:
                        notify::EventKind::Modify(
                            notify::event::ModifyKind::Name(..)
                            | notify::event::ModifyKind::Data(..),
                        )
                        | notify::EventKind::Create(notify::event::CreateKind::File)
                        | notify::EventKind::Remove(notify::event::RemoveKind::File),
                    paths: path_list,
                    ..
                }) => path_list.into_iter().all(|file| {
                    // NOTE: Check that this does affect Tera templates to avoid indirect self-recursion due to inplace hydration.
                    file.extension()
                        .map(|target_extension| target_extension.eq_ignore_ascii_case("tera"))
                        .unwrap_or(false)
                }),
                HydrationEvent::Filesystem(notify::Event { kind: _, .. }) => false,
                HydrationEvent::Context => true,
            }
        };

        if target_change {
            let mut change_closure = async || -> anyhow::Result<Vec<(PathBuf, PathBuf)>> {
                // NOTE: Under a compatible circumstance, reload the complete Tera state.
                Tera::full_reload(tera_state)?;

                let mut rename_list = Vec::new();

                let temp_filedir = tempdir_in(template_root.as_path())?;

                for template_name in tera_state.get_template_names() {
                    let mut template_path = template_root.join(template_name);

                    let template_perms = fs::metadata(template_path.as_path()).await?.permissions();

                    let mut temp_path = temp_filedir.path().join(template_name);

                    // NOTE: Remove the `*.tera` extension from both the template and atomic tempfile path.
                    template_path.set_extension("");
                    temp_path.set_extension("");

                    fs::create_dir_all(
                        temp_path
                            .parent()
                            .ok_or_else(|| anyhow!("tempfile does not have a parent"))?,
                    )
                    .await?;

                    fs::write(
                        temp_path.as_path(),
                        Tera::render(tera_state, template_name, &context_pipe.borrow())?.as_bytes(),
                    )
                    .await?;

                    fs::set_permissions(temp_path.as_path(), template_perms).await?;

                    rename_list.push((temp_path, template_path));
                }

                Ok(rename_list)
            };

            let target_value = change_closure().await;

            if let Ok(rename_list) = target_value {
                for (from, to) in rename_list {
                    fs::rename(from, to).await?;
                }
            } else if let Err(target_error) = target_value {
                eprintln!("failed to render tera state: {}", target_error)
            }
        }

        Ok(())
    }
}
