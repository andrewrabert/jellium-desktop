//! `IOSurface` import for CEF accelerated-paint frames.

use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTextureDescriptor, MTLTextureType,
    MTLTextureUsage,
};
use wgpu_hal::metal;

use crate::SharedTexture;
use crate::error::SurfaceLost;
use crate::shared::{ImportFailed, Imported, Opened};

/// A Mac has one system default Metal device, so there is nothing to name and
/// nothing to mismatch.
pub type ProducerId = ();

pub(crate) fn adapter_matches(_adapter: &wgpu::Adapter, _want: ProducerId) -> bool {
    true
}

pub(crate) fn open_device(adapter: &wgpu::Adapter) -> Result<Opened, SurfaceLost> {
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("jfn_gpu_paint device"),
        required_features: wgpu::Features::empty(),
        // Adapter limits — the swapchain may be larger than the downlevel
        // 2048×2048 cap on modern displays.
        required_limits: adapter.limits(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))?;
    Ok(Opened {
        device,
        queue,
        import_capable: true,
    })
}

/// One-deep cache keyed on the `IOSurfaceRef`'s identity **and extent**: CEF
/// recycles a small pool of surfaces, so consecutive frames usually arrive on
/// the one already wrapped.
///
/// The extent is part of the key because CEF hands the same address back at a
/// new size after a relayout; keying on identity alone samples a wrapper built
/// over the old size.
pub(crate) struct Importer {
    cached: Option<Cached>,
}

struct Cached {
    /// The `IOSurfaceRef` address. Compared, never dereferenced — a surface
    /// that went away cannot come back at the same address while CEF holds it.
    key: usize,
    size: (u32, u32),
    texture: wgpu::Texture,
}

impl Importer {
    pub(crate) fn new() -> Self {
        Self { cached: None }
    }

    pub(crate) fn import(
        &mut self,
        device: &wgpu::Device,
        frame: &SharedTexture,
    ) -> Result<Imported, ImportFailed> {
        let raw = frame.io_surface();
        if raw.is_null() {
            return Err(ImportFailed("null IOSurface"));
        }
        // SAFETY: `SharedTexture` only exists for a frame CEF handed us, and
        // the surface is live for the duration of the paint callback.
        let io_surface: &IOSurfaceRef = unsafe { &*raw.cast::<IOSurfaceRef>() };
        let width = u32::try_from(io_surface.width()).map_err(|_| ImportFailed("bad width"))?;
        let height = u32::try_from(io_surface.height()).map_err(|_| ImportFailed("bad height"))?;
        if width == 0 || height == 0 {
            return Err(ImportFailed("empty IOSurface"));
        }

        let key = raw as usize;
        if let Some(cached) = self.cached.as_ref()
            && cached.key == key
            && cached.size == (width, height)
        {
            return Ok(Imported {
                texture: cached.texture.clone(),
                acquire: None,
                reused: true,
            });
        }

        let (mtl_format, format) = formats(io_surface.pixel_format());
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                mtl_format,
                width as usize,
                height as usize,
                false,
            )
        };
        desc.setUsage(MTLTextureUsage::ShaderRead);
        desc.setStorageMode(MTLStorageMode::Shared);

        let hal_device =
            unsafe { device.as_hal::<metal::Api>() }.ok_or(ImportFailed("not a Metal device"))?;
        let mtl_texture = hal_device
            .raw_device()
            .newTextureWithDescriptor_iosurface_plane(&desc, io_surface, 0)
            .ok_or(ImportFailed("newTextureWithDescriptor:iosurface:plane:"))?;

        let hal_texture = unsafe {
            metal::Device::texture_from_raw(
                mtl_texture,
                format,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu_hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
            )
        };
        let texture = unsafe {
            device.create_texture_from_hal::<metal::Api>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("jfn_gpu_paint shared"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
            )
        };
        self.cached = Some(Cached {
            key,
            size: (width, height),
            texture: texture.clone(),
        });
        Ok(Imported {
            texture,
            acquire: None,
            reused: false,
        })
    }
}

/// `'RGBA'`; anything else takes the BGRA path this platform has always
/// assumed, rather than dropping a frame over an unrecognised tag.
const IO_SURFACE_RGBA: u32 = u32::from_be_bytes(*b"RGBA");

fn formats(pixel_format: u32) -> (MTLPixelFormat, wgpu::TextureFormat) {
    if pixel_format == IO_SURFACE_RGBA {
        (MTLPixelFormat::RGBA8Unorm, wgpu::TextureFormat::Rgba8Unorm)
    } else {
        (MTLPixelFormat::BGRA8Unorm, wgpu::TextureFormat::Bgra8Unorm)
    }
}

/// Metal has no queue-family ownership to transfer.
pub(crate) fn acquire_barrier(
    _device: &wgpu::Device,
    _encoder: &mut wgpu::CommandEncoder,
    _image: u64,
) {
}
