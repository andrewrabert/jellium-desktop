use crate::{BuildArgs, cef, fs as xfs, mpv, paths};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn stage_cef(out: &Path, cef: &cef::Cef) -> Result<()> {
    if cef.link_external {
        return Ok(());
    }
    xfs::copy_glob(&cef.dir, out, &["*.so*", "*.bin"])?;
    xfs::copy_glob(&cef.dir, out, &["*.pak", "*.dat"])?;
    xfs::copy_dir_recursive(&cef.dir.join("locales"), &out.join("locales"))?;
    Ok(())
}

pub fn stage_mpv_scripts(out: &Path) -> Result<()> {
    let src = paths::repo_root()
        .join("resources")
        .join("mpv")
        .join("scripts");
    if src.exists() {
        xfs::copy_dir_recursive(&src, &out.join("mpv").join("scripts"))?;
    }
    Ok(())
}

pub fn stage_mpv(out: &Path, _mpv: &mpv::Mpv, _used_external: bool, _bin: &Path) -> Result<()> {
    // libmpv is resolved from the system or the external prefix. One staged
    // by an older build would shadow it through the binary's $ORIGIN rpath.
    match std::fs::remove_file(out.join(mpv::runtime_library_name())) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

pub fn install(build_dir: &Path, prefix: &Path, args: &BuildArgs) -> Result<PathBuf> {
    let bin_src = build_dir.join("jellium-desktop");
    let bin_dst = prefix.join("jellium-desktop");
    xfs::copy_executable(&bin_src, &bin_dst)?;

    if args.cef_path.is_none() {
        let cef = cef::discover(&args.external_cef)?;
        xfs::copy_glob(&cef.dir, prefix, &["*.so*", "*.bin"])?;
        xfs::copy_glob(&cef.dir, prefix, &["*.pak", "*.dat"])?;
        xfs::copy_dir_recursive(&cef.dir.join("locales"), &prefix.join("locales"))?;
    }

    // With the system libmpv there is nothing to ship.
    if let Some(dir) = &args.external_mpv {
        xfs::copy_file(
            &dir.join("lib").join("libmpv.so"),
            &prefix.join("libmpv.so"),
        )?;
    }

    let scripts_src = paths::repo_root()
        .join("resources")
        .join("mpv")
        .join("scripts");
    if scripts_src.exists() {
        xfs::copy_dir_recursive(&scripts_src, &prefix.join("mpv").join("scripts"))?;
    }
    Ok(prefix.to_path_buf())
}
