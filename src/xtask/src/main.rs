use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod build;
#[cfg(target_os = "macos")]
mod bundle_macos;
mod cef;
mod fs;
mod install;
mod mpv;
mod package;
mod paths;
#[cfg_attr(target_os = "macos", path = "platform_macos.rs")]
#[cfg_attr(target_os = "windows", path = "platform_windows.rs")]
#[cfg_attr(
    all(not(target_os = "macos"), not(target_os = "windows")),
    path = "platform_linux.rs"
)]
mod platform;
#[cfg(target_os = "macos")]
mod template;
mod version;

#[derive(Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Build(BuildArgs),
    Install(InstallArgs),
    Package(PackageArgs),
    FetchCef,
    Version,
}

#[derive(clap::Args, Clone)]
pub struct BuildArgs {
    #[arg(long)]
    pub external_cef: Option<PathBuf>,
    #[arg(long)]
    pub cef_path: Option<PathBuf>,
    #[arg(long, env = "EXTERNAL_MPV_DIR")]
    pub external_mpv: Option<PathBuf>,
    #[arg(long)]
    pub mpv_cli: bool,
    #[arg(long)]
    pub no_kde_palette: bool,
    #[arg(long, default_value = "build")]
    pub out: PathBuf,
}

#[derive(clap::Args)]
pub struct InstallArgs {
    #[command(flatten)]
    pub build: BuildArgs,
    #[arg(long)]
    pub prefix: PathBuf,
    #[arg(long)]
    pub skip_build: bool,
}

#[derive(clap::Args)]
pub struct PackageArgs {
    #[command(flatten)]
    pub install: InstallArgs,
    #[arg(long, default_value = "dist")]
    pub dist: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Build(a) => build::run(&a).map(|_| ()),
        Cmd::Install(a) => install::run(&a).map(|_| ()),
        Cmd::Package(a) => package::run(&a),
        Cmd::FetchCef => {
            cef::ensure(&paths::cef_cache_dir()).map(|dir| println!("CEF ready: {}", dir.display()))
        }
        Cmd::Version => {
            println!("{}", version::read()?.full);
            Ok(())
        }
    }
}
