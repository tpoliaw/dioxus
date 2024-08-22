use super::web::install_web_build_tooling;
use super::BuildRequest;
use super::TargetPlatform;
// use crate::assets::create_assets_head;
// use crate::assets::{asset_manifest, process_assets};
use crate::assets::AssetManifest;
// use crate::assets::{copy_dir_to, AssetManifest};
use crate::builder::progress::build_cargo;
use crate::builder::progress::CargoBuildResult;
use crate::builder::progress::Stage;
use crate::builder::progress::UpdateBuildProgress;
use crate::builder::progress::UpdateStage;
use crate::config::Platform;
use crate::link::LinkCommand;
use crate::Result;
use anyhow::Context;
use futures_channel::mpsc::UnboundedSender;
use std::fs::create_dir_all;
use std::path::PathBuf;
use tokio::process::Command;

type ProgressChannel = UnboundedSender<UpdateBuildProgress>;

impl BuildRequest {
    /// Create a list of arguments for cargo builds
    pub(crate) fn build_arguments(&self) -> Vec<String> {
        let mut cargo_args = Vec::new();

        if self.build_arguments.release {
            cargo_args.push("--release".to_string());
        }
        if self.build_arguments.verbose {
            cargo_args.push("--verbose".to_string());
        } else {
            cargo_args.push("--quiet".to_string());
        }

        if let Some(custom_profile) = &self.build_arguments.profile {
            cargo_args.push("--profile".to_string());
            cargo_args.push(custom_profile.to_string());
        }

        if !self.build_arguments.target_args.features.is_empty() {
            let features_str = self.build_arguments.target_args.features.join(" ");
            cargo_args.push("--features".to_string());
            cargo_args.push(features_str);
        }

        if let Some(target) = self
            .targeting_web()
            .then_some("wasm32-unknown-unknown")
            .or(self.build_arguments.target_args.target.as_deref())
        {
            cargo_args.push("--target".to_string());
            cargo_args.push(target.to_string());
        }

        if let Some(ref platform) = self.build_arguments.target_args.package {
            cargo_args.push(String::from("-p"));
            cargo_args.push(platform.clone());
        }

        cargo_args.append(&mut self.build_arguments.cargo_args.clone());

        match self.dioxus_crate.executable_type() {
            krates::cm::TargetKind::Bin => {
                cargo_args.push("--bin".to_string());
            }
            krates::cm::TargetKind::Lib => {
                cargo_args.push("--lib".to_string());
            }
            krates::cm::TargetKind::Example => {
                cargo_args.push("--example".to_string());
            }
            _ => {}
        };
        cargo_args.push(self.dioxus_crate.executable_name().to_string());

        cargo_args
    }

    /// Create a build command for cargo
    fn prepare_build_command(&self) -> Result<(Command, Vec<String>)> {
        let mut cmd = Command::new("cargo");
        cmd.arg("rustc");
        if let Some(target_dir) = &self.target_dir {
            cmd.env("CARGO_TARGET_DIR", target_dir);
        }
        cmd.current_dir(self.dioxus_crate.crate_dir())
            .arg("--message-format")
            .arg("json-diagnostic-rendered-ansi");

        let cargo_args = self.build_arguments();
        cmd.args(&cargo_args);

        cmd.arg("--").args(self.rust_flags.clone());

        Ok((cmd, cargo_args))
    }

    pub(crate) async fn build(self, mut progress: ProgressChannel) -> Result<BuildRequest> {
        tracing::info!("🚅 Running build [Desktop] command...");

        // Set up runtime guards
        let mut dioxus_version = crate::dx_build_info::PKG_VERSION.to_string();
        if let Some(hash) = crate::dx_build_info::GIT_COMMIT_HASH_SHORT {
            let hash = &hash.trim_start_matches('g')[..4];
            dioxus_version.push_str(&format!("-{hash}"));
        }

        // If this is a web, build make sure we have the web build tooling set up
        if self.targeting_web() {
            install_web_build_tooling(&mut progress).await?;
        }

        // Create the build command
        let (cmd, cargo_args) = self.prepare_build_command()?;

        // Run the build command with a pretty loader
        let crate_count = self.get_unit_count_estimate().await;
        let cargo_result = build_cargo(crate_count, cmd, &mut progress).await?;

        // Post process the build result
        self.post_process_build(cargo_args, &cargo_result, &mut progress)
            .await
            .context("Failed to post process build")?;

        tracing::info!(
            "🚩 Build completed: [{}]",
            self.dioxus_crate.out_dir().display()
        );

        _ = progress.start_send(UpdateBuildProgress {
            stage: Stage::Finished,
            update: UpdateStage::Start,
        });

        Ok(self)
    }

    async fn post_process_build(
        &self,
        cargo_args: Vec<String>,
        cargo_build_result: &CargoBuildResult,
        progress: &mut UnboundedSender<UpdateBuildProgress>,
    ) -> Result<()> {
        _ = progress.start_send(UpdateBuildProgress {
            stage: Stage::OptimizingAssets,
            update: UpdateStage::Start,
        });

        let assets = self.collect_assets(cargo_args, progress).await?;

        let file_name = self.dioxus_crate.executable_name();

        // Move the final output executable into the dist folder
        let out_dir = self.target_out_dir();
        if !out_dir.is_dir() {
            create_dir_all(&out_dir)?;
        }

        let mut output_path = out_dir.join(file_name);

        // todo: this should not be platform cfged but rather be a target config
        // we dont always want to set the .exe extension...
        if self.targeting_web() {
            output_path.set_extension("wasm");
        } else if cfg!(windows) {
            output_path.set_extension("exe");
        }

        if let Some(res_path) = &cargo_build_result.output_location {
            std::fs::copy(res_path, &output_path)?;
        }

        self.copy_assets_dir()?;

        // If this is a web build, run web post processing steps
        if self.targeting_web() {
            self.post_process_web_build(assets.as_ref(), progress)
                .await?;
        }

        Ok(())
    }

    /// Get the output directory for a specific built target
    pub fn target_out_dir(&self) -> PathBuf {
        let out_dir = self.dioxus_crate.out_dir();
        match self.build_arguments.platform {
            Some(Platform::Fullstack | Platform::StaticGeneration) => match self.target_platform {
                TargetPlatform::Web => out_dir.join("public"),
                TargetPlatform::Desktop => out_dir.join("desktop"),
                _ => out_dir,
            },
            _ => out_dir,
        }
    }
}
