//! Decodes the shell overlay's logo at build time, so no codec runs in the
//! app: the binary carries the pixels the GPU uploads.

use std::io::{BufReader, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let asset = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/logo.png");
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
