//! Build system & asset pipelines.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_std::fs;
use async_std::path::Path;
use futures::channel::mpsc::Sender;
use futures::stream::StreamExt;
use indicatif::ProgressBar;

use crate::common::{copy_dir_recursive, remove_dir_all, BUILDING, ERROR, SUCCESS};
use crate::config::RtcBuild;
use crate::pipelines::HtmlPipeline;

/// A system used for building a Rust WASM app & bundling its assets.
///
/// This unit of data should be used throughout the system for driving build processes and
/// bundling tasks. Different CLI commands which need to trigger builds in some way should
/// be able to gather the needed data to create an instance of this struct, and then the vairous
/// build routines can be cleanly abstracted away form any specific CLI endpoints.
pub struct BuildSystem {
    /// Runtime config.
    cfg: Arc<RtcBuild>,
    /// HTML build pipeline.
    html_pipeline: Arc<HtmlPipeline>,
    /// The build system progress bar for displaying the state of the build system overall.
    progress: ProgressBar,
}

impl BuildSystem {
    /// Create a new instance from the raw components.
    ///
    /// Reducing the number of assumptions here should help us to stay flexible when adding new
    /// commands, rafctoring and the like.
    pub async fn new(cfg: Arc<RtcBuild>, progress: ProgressBar, ignore_chan: Option<Sender<PathBuf>>) -> Result<Self> {
        let html_pipeline = Arc::new(HtmlPipeline::new(cfg.clone(), progress.clone(), ignore_chan)?);
        Ok(Self {
            cfg,
            html_pipeline,
            progress,
        })
    }

    /// Build the application described in the given build data.
    pub async fn build(&mut self) -> Result<()> {
        self.progress.reset();
        self.progress.enable_steady_tick(100);
        self.progress.set_prefix(&format!("{}", BUILDING));
        self.progress.set_message("starting build");
        let res = self.do_build().await;
        self.progress.disable_steady_tick();
        self.progress.set_position(0);
        match res {
            Ok(_) => {
                self.progress.set_prefix(&format!("{}", SUCCESS));
                self.progress.finish_with_message("success");
                Ok(())
            }
            Err(err) => {
                self.progress.set_prefix(&format!("{}", ERROR));
                self.progress.finish_with_message("error");
                Err(err)
            }
        }
    }

    /// Creates a "staging area" (dist/.stage) for storing intermediate build results
    async fn prepare_staging_dist(&self) -> Result<()> {
        // Prepare staging area in which we will assemble the latest build
        let staging_dist: &Path = self.cfg.staging_dist.as_path().into();

        // Clean staging area, if applicable
        let mut entries = fs::read_dir(staging_dist).await.context("error reading staging dist dir")?;
        while let Some(entry) = entries.next().await {
            let entry = entry.context("error reading contents of staging dist dir")?;
            let file_type = entry.file_type().await.context("error reading metadata of file in staging dist dir")?;

            if file_type.is_dir() {
                fs::remove_dir_all(entry.path()).await.context("Cleaning staging dist dir failed")?;
            } else if file_type.is_symlink() || file_type.is_file() {
                fs::remove_file(entry.path()).await.context("Cleaning staging dist dir failed")?;
            }
        }

        Ok(())
    }

    async fn do_build(&mut self) -> Result<()> {
        // Ensure the output dist directories are in place.
        fs::create_dir_all(self.cfg.final_dist.as_path())
            .await
            .with_context(|| "error creating build environment directory: dist")?;
        fs::create_dir_all(self.cfg.staging_dist.as_path())
            .await
            .with_context(|| "error creating build environment directory: staging dist dir")?;

        self.prepare_staging_dist().await.context("error preparing build environment")?;

        // Spawn the source HTML pipeline. This will spawn all other pipelines derived from
        // the source HTML, and will ultimately generate and write the final HTML.
        self.html_pipeline.clone().spawn().await?;

        // Move distrbution from staging dist to final dist
        self.finalize_dist().await.context("error applying built distribution")?;
        Ok(())
    }

    /// Moves the contents of dist/.stage into dist, signifying the application
    /// of a successful build. Also removes dist/.stage afterwards.
    async fn finalize_dist(&self) -> Result<()> {
        let final_dist = self.cfg.final_dist.clone();
        let staging_dist = self.cfg.staging_dist.clone();
        self.progress.clone().set_message("applying new distribution");

        // Build succeeded, so delete everything in `dist`,
        // copy everything from `dist/.stage` to `dist`, and
        // then delete `dist/.stage`.
        let mut entries = fs::read_dir(&final_dist).await.context("error reading final dist dir")?;
        while let Some(entry) = entries.next().await {
            let entry = entry.context("error reading contents of final dist dir")?;
            if entry.file_name() == ".stage" {
                continue;
            }

            let file_type = entry.file_type().await.context("error reading metadata of file in final dist dir")?;

            if file_type.is_dir() {
                remove_dir_all(entry.path().into()).await.context("error cleaning final dist dir")?;
            } else if file_type.is_symlink() || file_type.is_file() {
                fs::remove_file(entry.path()).await.context("error cleaning final dist dir")?;
            }
        }

        copy_dir_recursive(staging_dist.to_path_buf(), final_dist.to_path_buf())
            .await
            .context("error copying staging dist dir to final dist dir")?;

        remove_dir_all(staging_dist).await.context("error deleting staging dist dir")?;

        Ok(())
    }
}
