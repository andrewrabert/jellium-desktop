use crate::{BuildArgs, cef, fs as xfs, mpv, paths, version};
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// CEF and mpv resolved for one cargo invocation, together with the
/// environment they imply. Builds the in-tree mpv when `--external-mpv`
/// names none.
pub struct Toolchain {
    pub cef: cef::Cef,
    pub mpv: mpv::Mpv,
    pub external_mpv: bool,
    /// Keeps the linked SDK proxy alive for as long as any command built
    /// from this toolchain can run.
    cef_proxy: Option<(tempfile::TempDir, PathBuf)>,
}

/// The environment variable the platform's runtime loader searches.
const LOADER_PATH_VAR: &str = if cfg!(target_os = "macos") {
    "DYLD_LIBRARY_PATH"
} else if cfg!(target_os = "windows") {
    "PATH"
} else {
    "LD_LIBRARY_PATH"
};

impl Toolchain {
    pub fn resolve(args: &BuildArgs, out: &Path) -> Result<Toolchain> {
        let cef_info = match &args.cef_path {
            Some(dir) => cef::explicit(dir)?,
            None => cef::discover(&args.external_cef)?,
        };
        println!("Found CEF: {}", cef_info.version);

        let (mpv_info, external_mpv) = if let Some(dir) = &args.external_mpv {
            println!("Using external mpv from: {}", dir.display());
            (mpv::external(dir)?, true)
        } else {
            (mpv::build(out, args.mpv_cli)?, false)
        };

        let cef_proxy = if cef_info.link_external {
            Some(cef::sdk_proxy(&cef_info.root)?)
        } else {
            None
        };

        Ok(Toolchain {
            cef: cef_info,
            mpv: mpv_info,
            external_mpv,
            cef_proxy,
        })
    }

    /// The directory holding the mpv shared library at runtime.
    fn mpv_lib_dir(&self, args: &BuildArgs) -> PathBuf {
        match &args.external_mpv {
            Some(dir) => dir.join("lib"),
            None => self.mpv.build_dir.clone(),
        }
    }

    /// `cargo <subcommand>` carrying CEF, mpv, git and target-dir
    /// environment, plus the mpv library directory on the platform's
    /// runtime loader path so a test binary starts.
    pub fn cargo(&self, args: &BuildArgs, out: &Path, subcommand: &str) -> Command {
        let mut cmd = Command::new("cargo");
        cmd.arg(subcommand)
            .arg("--manifest-path")
            .arg(paths::workspace_manifest());
        if args.no_kde_palette {
            cmd.arg("--no-default-features");
        }
        cmd.env("CARGO_TARGET_DIR", paths::cargo_target_dir(out));

        match &self.cef_proxy {
            Some((_tmp, proxy)) => {
                cmd.env("CEF_PATH", proxy);
                cmd.env("CEF_RESOURCES_DIR", &self.cef.root);
            }
            None => {
                cmd.env("CEF_PATH", &self.cef.root);
                cmd.env_remove("CEF_RESOURCES_DIR");
            }
        }

        // Single source of truth for the embedded commit hash. xtask always
        // runs (never cargo-cached), so it recomputes every build; the build
        // scripts read these via cargo:rerun-if-env-changed for exact
        // invalidation.
        let (git_hash, git_dirty) = version::git_info();
        cmd.env("JFN_GIT_HASH", git_hash.unwrap_or_default());
        cmd.env("JFN_GIT_DIRTY", if git_dirty { "1" } else { "0" });

        if let Some(dir) = &args.external_mpv {
            cmd.env("EXTERNAL_MPV_DIR", dir);
            cmd.env_remove("JFN_MPV_INCLUDE_DIR");
            cmd.env_remove("JFN_MPV_LIB_DIR");
        } else {
            cmd.env_remove("EXTERNAL_MPV_DIR");
            cmd.env(
                "JFN_MPV_INCLUDE_DIR",
                paths::mpv_source_dir().join("include"),
            );
            cmd.env("JFN_MPV_LIB_DIR", &self.mpv.build_dir);
        }

        // Linux: rpath system / out-of-tree lib dirs into the binary so it
        // resolves DT_NEEDED entries that aren't shipped alongside it.
        // In-tree builds (.cache/cef + meson mpv) stay relocatable —
        // libs are staged next to the binary and $ORIGIN handles them.
        if cfg!(target_os = "linux") {
            let mut rpaths: Vec<String> = Vec::new();
            if self.cef.link_external {
                rpaths.push(self.cef.dir.to_string_lossy().into_owned());
            }
            if let Some(dir) = &args.external_mpv {
                rpaths.push(dir.join("lib").to_string_lossy().into_owned());
            }
            if rpaths.is_empty() {
                cmd.env_remove("JFN_EXTRA_RPATH");
            } else {
                cmd.env("JFN_EXTRA_RPATH", rpaths.join(":"));
            }
        }

        let mut loader_path = OsString::from(self.mpv_lib_dir(args));
        if let Some(existing) = std::env::var_os(LOADER_PATH_VAR)
            && !existing.is_empty()
        {
            loader_path.push(if cfg!(target_os = "windows") {
                ";"
            } else {
                ":"
            });
            loader_path.push(existing);
        }
        cmd.env(LOADER_PATH_VAR, loader_path);

        cmd
    }
}

pub fn run(args: &BuildArgs) -> Result<()> {
    let out = std::path::absolute(&args.out)?;
    std::fs::create_dir_all(&out)?;

    let toolchain = Toolchain::resolve(args, &out)?;

    let mut cmd = toolchain.cargo(args, &out, "build");
    cmd.arg("--release").arg("--bin").arg("jellium-desktop");

    println!("Building jellium-desktop (Rust binary)...");
    let status = cmd.status().context("spawn cargo build")?;
    if !status.success() {
        bail!("cargo build failed");
    }

    let bin_name = if cfg!(target_os = "windows") {
        "jellium-desktop.exe"
    } else {
        "jellium-desktop"
    };
    let bin_src = paths::cargo_target_dir(&out).join("release").join(bin_name);
    let bin_dst = out.join(bin_name);
    xfs::copy_file(&bin_src, &bin_dst)?;

    crate::platform::stage_cef(&out, &toolchain.cef)?;
    crate::platform::stage_mpv(&out, &toolchain.mpv, toolchain.external_mpv, &bin_dst)?;
    Ok(())
}
