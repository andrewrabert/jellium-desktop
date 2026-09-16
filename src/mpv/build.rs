use std::env;
use std::error::Error;
use std::path::PathBuf;

type BuildResult<T> = Result<T, Box<dyn Error>>;

fn main() -> BuildResult<()> {
    println!("cargo:rerun-if-env-changed=JFN_MPV_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=JFN_MPV_LIB_DIR");
    println!("cargo:rerun-if-env-changed=EXTERNAL_MPV_DIR");

    let (include_dirs, linked_via_pkgconfig) = resolve_paths()?;
    let header = locate_header(&include_dirs)?;
    println!("cargo:rerun-if-changed={}", header.display());

    if !linked_via_pkgconfig {
        println!("cargo:rustc-link-lib=mpv");
    }

    let mut builder = bindgen::Builder::default()
        .header(header.to_string_lossy().to_string())
        .allowlist_function("mpv_.*")
        .allowlist_type("mpv_.*")
        .allowlist_var("MPV_.*")
        .blocklist_type("mpv_node_list")
        .blocklist_type("mpv_byte_array")
        .newtype_enum("mpv_event_id")
        .newtype_enum("mpv_format")
        .newtype_enum("mpv_log_level")
        .newtype_enum("mpv_error")
        .newtype_enum("mpv_end_file_reason")
        .derive_debug(true)
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for dir in &include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }

    let bindings = builder.generate()?;

    let out_path = PathBuf::from(env::var("OUT_DIR")?).join("bindings.rs");
    bindings.write_to_file(&out_path)?;

    generate_avcodec_bindings()?;
    Ok(())
}

fn generate_avcodec_bindings() -> BuildResult<()> {
    println!("cargo:rerun-if-env-changed=EXTERNAL_AVCODEC_DIR");

    let mut include_dirs: Vec<PathBuf> = Vec::new();
    let mut linked_via_pkgconfig = false;

    if let Ok(dir) = env::var("EXTERNAL_AVCODEC_DIR") {
        let root = PathBuf::from(&dir);
        include_dirs.push(root.join("include"));
        let libdir = root.join("lib");
        println!("cargo:rustc-link-search=native={}", libdir.display());
        println!("cargo:rustc-link-lib=avcodec");
    } else if let Ok(dir) = env::var("EXTERNAL_MPV_DIR") {
        let root = PathBuf::from(&dir);
        let candidate = root.join("include").join("libavcodec").join("avcodec.h");
        if candidate.exists() {
            include_dirs.push(root.join("include"));
            let libdir = root.join("lib");
            println!("cargo:rustc-link-search=native={}", libdir.display());
            println!("cargo:rustc-link-lib=avcodec");
        }
    }

    if include_dirs.is_empty() {
        let lib = pkg_config::Config::new().probe("libavcodec")?;
        include_dirs.extend(lib.include_paths.iter().cloned());
        linked_via_pkgconfig = true;
    }
    let _ = linked_via_pkgconfig;

    let header_dir = include_dirs
        .iter()
        .find(|p| p.join("libavcodec/avcodec.h").exists())
        .cloned()
        .ok_or_else(|| {
            format!(
                "could not locate libavcodec/avcodec.h in any of: {:?}\n\
                 Set EXTERNAL_AVCODEC_DIR, EXTERNAL_MPV_DIR (with ffmpeg \
                 headers under include/), or install libavcodec via pkg-config.",
                include_dirs
            )
        })?;
    let header = header_dir.join("libavcodec/avcodec.h");
    println!("cargo:rerun-if-changed={}", header.display());

    let mut builder = bindgen::Builder::default()
        .header(header.to_string_lossy().to_string())
        .allowlist_function("av_codec_iterate")
        .allowlist_function("av_codec_is_decoder")
        .allowlist_function("avcodec_get_name")
        .allowlist_type("AVCodec")
        .allowlist_type("AVCodecID")
        .allowlist_type("AVMediaType")
        .newtype_enum("AVMediaType")
        .newtype_enum("AVCodecID")
        .derive_debug(true)
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));

    for dir in &include_dirs {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
    }

    let bindings = builder.generate()?;

    let out_path = PathBuf::from(env::var("OUT_DIR")?).join("avcodec_bindings.rs");
    bindings.write_to_file(&out_path)?;
    Ok(())
}

fn resolve_paths() -> BuildResult<(Vec<PathBuf>, bool)> {
    let mut includes: Vec<PathBuf> = Vec::new();
    let mut linked = false;

    if let Ok(dir) = env::var("JFN_MPV_INCLUDE_DIR") {
        includes.push(PathBuf::from(dir));
    }

    if let Ok(dir) = env::var("JFN_MPV_LIB_DIR") {
        println!("cargo:rustc-link-search=native={dir}");
    }

    if let Ok(dir) = env::var("EXTERNAL_MPV_DIR") {
        let root = PathBuf::from(&dir);
        includes.push(root.join("include"));
        let libdir = root.join("lib");
        println!("cargo:rustc-link-search=native={}", libdir.display());
    }

    if let Ok(lib) = pkg_config::Config::new()
        .atleast_version("0.37")
        .probe("mpv")
    {
        for p in &lib.include_paths {
            includes.push(p.clone());
        }
        linked = true;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let vendored = manifest.join("../../third_party/mpv/include");
    if vendored.exists() {
        includes.push(vendored);
    }

    Ok((includes, linked))
}

fn locate_header(include_dirs: &[PathBuf]) -> BuildResult<PathBuf> {
    for dir in include_dirs {
        let candidate = dir.join("mpv").join("client.h");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not locate mpv/client.h in any of: {:?}\n\
         Set JFN_MPV_INCLUDE_DIR, EXTERNAL_MPV_DIR, or install libmpv via pkg-config.",
        include_dirs
    )
    .into())
}
