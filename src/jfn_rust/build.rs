fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    decode_shell_logo()?;

    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
        println!("cargo:rustc-link-arg-bins=-Wl,--disable-new-dtags");
        println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic");

        println!("cargo:rerun-if-env-changed=JFN_EXTRA_RPATH");
        if let Ok(extra) = std::env::var("JFN_EXTRA_RPATH") {
            for entry in extra.split(':').filter(|s| !s.is_empty()) {
                println!("cargo:rustc-link-arg-bins=-Wl,-rpath,{entry}");
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        use std::path::PathBuf;

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or("CARGO_MANIFEST_DIR has no grandparent")?;
        let rc_template = repo_root
            .join("resources")
            .join("win")
            .join("iconres.rc.in");
        println!("cargo:rerun-if-changed={}", rc_template.display());

        let template = std::fs::read_to_string(&rc_template)?;

        println!("cargo:rerun-if-changed=../Cargo.toml");
        let version = env!("CARGO_PKG_VERSION").to_string();
        let numeric: Vec<&str> = version.split('-').next().unwrap_or("").split('.').collect();
        let mut major: u32 = numeric.first().and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut minor: u32 = numeric.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let mut patch: u32 = numeric.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        let fileflags = if version.contains('-') {
            major = 0;
            minor = 0;
            patch = 0;
            "VS_FF_PRERELEASE"
        } else {
            "0x0L"
        };
        println!("cargo:rerun-if-env-changed=JFN_GIT_HASH");
        println!("cargo:rerun-if-env-changed=JFN_GIT_DIRTY");
        let (git_hash, dirty) = match std::env::var("JFN_GIT_HASH") {
            Ok(h) if !h.is_empty() => {
                let dirty = std::env::var("JFN_GIT_DIRTY").as_deref() == Ok("1");
                (h, dirty)
            }
            _ => git_info(repo_root),
        };
        let version_full = if !version.contains('-') || git_hash.is_empty() {
            version.clone()
        } else if dirty {
            format!("{version}+{git_hash}-dirty")
        } else {
            format!("{version}+{git_hash}")
        };
        track_git_refs(repo_root);

        let cmake_source_dir = repo_root.to_string_lossy().replace('\\', "/");
        let expanded = template
            .replace("@APP_VERSION_MAJOR@", &major.to_string())
            .replace("@APP_VERSION_MINOR@", &minor.to_string())
            .replace("@APP_VERSION_PATCH@", &patch.to_string())
            .replace("@APP_VERSION_FILEFLAGS@", fileflags)
            .replace("@APP_VERSION_FULL@", &version_full)
            .replace("@CMAKE_SOURCE_DIR@", &cmake_source_dir);

        let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
        let rc_out = out_dir.join("iconres.rc");
        std::fs::write(&rc_out, expanded)?;

        embed_resource::compile(&rc_out, embed_resource::NONE).manifest_required()?;

        println!("cargo:rustc-link-arg-bins=/SUBSYSTEM:WINDOWS");
        println!("cargo:rustc-link-arg-bins=/ENTRY:mainCRTStartup");
    }

    Ok(())
}

#[cfg(target_os = "windows")]
fn git_info(repo_root: &std::path::Path) -> (String, bool) {
    let Ok(repo) = gix::discover(repo_root) else {
        return (String::new(), false);
    };
    let hash = repo
        .head_id()
        .ok()
        .map(|id| id.to_hex_with_len(7).to_string())
        .unwrap_or_default();
    let dirty = repo.is_dirty().unwrap_or(false);
    (hash, dirty)
}

#[cfg(target_os = "windows")]
fn track_git_refs(repo_root: &std::path::Path) {
    let Ok(repo) = gix::discover(repo_root) else {
        return;
    };
    println!(
        "cargo:rerun-if-changed={}",
        repo.git_dir().join("HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo.common_dir().join("packed-refs").display()
    );
    if let Ok(Some(r)) = repo.head_ref() {
        let name = r.name().as_bstr().to_string();
        println!(
            "cargo:rerun-if-changed={}",
            repo.common_dir().join(name).display()
        );
    }
}

fn decode_shell_logo() -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufReader, Write};

    let asset = concat!(env!("CARGO_MANIFEST_DIR"), "/src/shell/assets/logo.png");
    println!("cargo:rerun-if-changed={asset}");

    let decoder = png::Decoder::new(BufReader::new(std::fs::File::open(asset)?));
    let mut reader = decoder.read_info()?;
    let info = reader.info();
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "{asset}: expected 8-bit RGBA, found {:?} at {:?}",
            info.color_type, info.bit_depth
        )
        .into());
    }
    let (width, height) = (info.width, info.height);

    let mut pixels = vec![
        0u8;
        reader
            .output_buffer_size()
            .ok_or("logo is too large to decode")?
    ];
    let frame = reader.next_frame(&mut pixels)?;
    pixels.truncate(frame.buffer_size());

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(out.join("logo.rgba"), &pixels)?;
    let mut dimensions = std::fs::File::create(out.join("logo_dimensions.rs"))?;
    writeln!(dimensions, "const WIDTH: u32 = {width};")?;
    writeln!(dimensions, "const HEIGHT: u32 = {height};")?;
    Ok(())
}
