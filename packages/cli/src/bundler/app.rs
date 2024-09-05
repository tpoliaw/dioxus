use std::path::PathBuf;

use crate::link::InterceptedArgs;
use crate::Result;
use crate::{
    assets::{copy_dir_to, AssetManifest},
    link::LINK_OUTPUT_ENV_VAR,
};
use anyhow::Context;
use core::str;
use futures_channel::mpsc::UnboundedSender;
use manganis_core::ResourceAsset;
use rayon::prelude::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator};
use std::process::Stdio;
use std::{
    env::current_exe,
    fs::{self, create_dir_all},
    io::Read,
    sync::{atomic::AtomicUsize, Arc},
};
use std::{
    io::{BufWriter, Write},
    path::Path,
};
use tokio::process::Command;
use tracing::Level;

use super::*;
use crate::DioxusCrate;
use crate::{
    build::BuildArgs,
    builder::{BuildRequest, Platform},
    config::BundleConfig,
};

pub struct AppBundle {
    pub build: BuildRequest,
    pub workdir: PathBuf,
    pub executable: PathBuf,
    pub assets: AssetManifest,
}

impl AppBundle {
    pub async fn new(
        build: BuildRequest,
        assets: AssetManifest,
        executable: PathBuf,
    ) -> Result<Self> {
        let bundle = Self {
            workdir: std::env::temp_dir(),
            build,
            executable,
            assets,
        };

        bundle.write_main_executable().await?;
        bundle.write_assets().await?;
        bundle.write_metadata().await?;
        bundle.optimize().await?;

        Ok(bundle)
    }

    pub fn open(&self) {}

    /// Take the workdir and copy it to the output location, returning the path to final bundle
    ///
    /// Perform any finishing steps here:
    /// - Signing the bundle
    pub async fn finish(&self, destination: PathBuf) -> Result<PathBuf> {
        match self.build.platform() {
            // Web is a simple fs copy of the workdir to the output location
            Platform::Web => {
                let output_location = destination.join(self.build.app_name());
                copy_dir_to(self.workdir.clone(), output_location.clone(), false)?;
                Ok(output_location)
            }

            // Create a .ipa, only from macOS
            Platform::Ios => todo!(),

            // Create a .exe, from linux/mac/windows
            Platform::Android => todo!(),

            // Create a final .app/.exe/etc depending on the host platform, not dependent on the host
            Platform::Desktop => todo!(),
            Platform::Server => todo!(),
            Platform::Liveview => todo!(),
        }
    }

    /// Take the output of rustc and make it into the main exe of the bundle
    ///
    /// For wasm, we'll want to run `wasm-bindgen` to make it a wasm binary along with some other optimizations
    /// Other platforms we might do some stripping or other optimizations
    async fn write_main_executable(&self) -> Result<()> {
        match self.build.platform() {
            // Run wasm-bindgen on the wasm binary and set its output to be in the bundle folder
            // Also run wasm-opt on the wasm binary, and sets the index.html since that's also the "executable".
            //
            // The wasm stuff will be in a folder called "wasm" in the workdir.
            //
            // Final output format:
            // ```
            // dist/
            //     web
            //         index.html
            //         wasm/
            //            app.wasm
            //            glue.js
            //            snippets/
            //                ...
            //         assets/
            //            logo.png
            // ```
            Platform::Web => {
                // Run wasm-bindgen and drop its output into the assets folder under "dioxus"
                self.build
                    .run_wasm_bindgen(&self.executable.with_extension("wasm"), &self.bindgen_dir())
                    .await?;

                // Only run wasm-opt if the feature is enabled
                // Wasm-opt has an expensive build script that makes it annoying to keep enabled for iterative dev
                // We put it behind the "wasm-opt" feature flag so that it can be disabled when iterating on the cli
                self.build.run_wasm_opt(&self.bindgen_dir())?;

                // Write the index.html file
                std::fs::write(self.workdir.join("index.html"), self.build.prepare_html()?)?;
            }

            Platform::Ios => {}
            Platform::Desktop => {}
            Platform::Server => {}
            Platform::Liveview => {}
            Platform::Android => todo!("android not yet supported!"),
        }

        Ok(())
    }

    fn bindgen_dir(&self) -> PathBuf {
        self.workdir.join("wasm")
    }

    /// Copy the assets out of the manifest and into the target location
    ///
    /// Should be the same on all platforms - just copy over the assets from the manifest into the output directory
    async fn write_assets(&self) -> Result<()> {
        let asset_dir = self.asset_dir();

        let assets = self.all_source_assets();
        let asset_count = assets.len();
        let assets_finished = AtomicUsize::new(0);

        let optimize = false;
        let pre_compress = false;

        // Parallel Copy over the assets and keep track of progress with an atomic counter
        assets.par_iter().try_for_each(|asset| {
            self.build.status_copying_asset(
                assets_finished.fetch_add(0, std::sync::atomic::Ordering::SeqCst),
                asset_count,
                asset,
            );

            self.assets
                .copy_asset_to(&asset_dir, asset, optimize, pre_compress);

            self.build.status_finished_asset(
                assets_finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                asset_count,
                asset,
            );

            Ok(()) as anyhow::Result<()>
        })?;

        Ok(())
    }

    pub fn all_source_assets(&self) -> Vec<PathBuf> {
        // Merge the legacy asset dir assets with the assets from the manifest
        // Legacy assets need to retain their name in case they're referenced in the manifest
        // todo: we should only copy over assets that appear in `img { src: "assets/logo.png" }` to
        // properly deprecate the legacy asset dir
        self.assets
            .assets
            .keys()
            .cloned()
            .chain(self.build.krate.legacy_asset_dir_files())
            .collect::<Vec<_>>()
    }

    async fn write_metadata(&self) -> Result<()> {
        Ok(())
    }

    pub fn asset_dir(&self) -> PathBuf {
        let dir: PathBuf = match self.build.platform() {
            Platform::Web => self.workdir.join("assets"),
            Platform::Desktop => self.workdir.join("Resources"),
            Platform::Ios => self.workdir.join("Resources"),
            Platform::Android => self.workdir.join("assets"),
            Platform::Server => self.workdir.join("assets"),
            Platform::Liveview => self.workdir.join("assets"),
        };

        if !dir.exists() {
            std::fs::create_dir_all(&dir).expect("Failed to create asset dir in temp dir");
        }

        dir
    }

    /// Run the optimizers, obfuscators, minimizers, etc
    pub async fn optimize(&self) -> Result<()> {
        match self.build.platform() {
            Platform::Web => {
                // Compress the asset dir
                // // If pre-compressing is enabled, we can pre_compress the wasm-bindgen output
                // let pre_compress = self
                //     .krate
                //     .should_pre_compress_web_assets(self.build.release);

                // tokio::task::spawn_blocking(move || {
                //     pre_compress_folder(&bindgen_outdir, pre_compress)
                // })
                // .await
                // .unwrap()?;
            }
            Platform::Desktop => {}
            Platform::Ios => {}
            Platform::Android => {}
            Platform::Server => {}
            Platform::Liveview => {}
        }

        Ok(())
    }

    /// The folder where the bundles will be built
    ///
    /// ```
    /// dist/
    ///   app-windows-11-arch-x64.exe
    ///   app-macos-11-arch-x64.app
    ///   app-macos-11-installer.dmg
    ///   app-linux-11-arch-x64.AppImage
    ///   server.sh
    ///   web
    ///     index.html
    ///     assets
    ///       logo.png
    /// ```
    fn bundle_root(&self) -> PathBuf {
        todo!()
    }
}
