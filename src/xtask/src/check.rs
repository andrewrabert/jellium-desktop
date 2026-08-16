use crate::BuildArgs;
use crate::build::Toolchain;
use anyhow::{Context, Result, bail};

/// `cargo clippy --workspace --all-targets` with the project's denies:
/// warnings, `clippy::unwrap_used`, `clippy::expect_used`, `clippy::panic`.
pub fn clippy(args: &BuildArgs) -> Result<()> {
    let out = std::path::absolute(&args.out)?;
    std::fs::create_dir_all(&out)?;
    let toolchain = Toolchain::resolve(args, &out)?;

    let mut cmd = toolchain.cargo(args, &out, "clippy");
    cmd.arg("--workspace").arg("--all-targets").args([
        "--",
        "-D",
        "warnings",
        "-D",
        "clippy::unwrap_used",
        "-D",
        "clippy::expect_used",
        "-D",
        "clippy::panic",
    ]);

    let status = cmd.status().context("spawn cargo clippy")?;
    if !status.success() {
        bail!("cargo clippy failed");
    }
    Ok(())
}

/// `cargo test --workspace`.
pub fn test(args: &BuildArgs) -> Result<()> {
    let out = std::path::absolute(&args.out)?;
    std::fs::create_dir_all(&out)?;
    let toolchain = Toolchain::resolve(args, &out)?;

    let mut cmd = toolchain.cargo(args, &out, "test");
    cmd.arg("--workspace");

    let status = cmd.status().context("spawn cargo test")?;
    if !status.success() {
        bail!("cargo test failed");
    }
    Ok(())
}
