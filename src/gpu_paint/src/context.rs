//! The process's one wgpu device, shared across surfaces.

use wgpu_hal::vulkan;

use crate::error::{Kind, SurfaceLost};
use crate::painter::Surface;
use crate::shared_import;
use crate::types::WindowTarget;
use jfn_platform_abi::PhysicalSize;

/// The only handle to wgpu in the process. Held once; [`crate::Surface`]s
/// borrow it.
pub struct Surfaces {
    pub(crate) instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    // wgpu-core's surface.configure drains the whole device queue and errors if
    // another thread submits mid-drain, leaving the surface unconfigured → next
    // acquire fatally panics. Configure takes the write side, submit the read.
    pub(crate) submit_gate: parking_lot::RwLock<()>,
    can_import_shared: bool,
}

impl Surfaces {
    pub(crate) fn configure_surface(
        &self,
        surface: &wgpu::Surface<'static>,
        config: &wgpu::SurfaceConfiguration,
    ) {
        let _guard = self.submit_gate.write();
        surface.configure(&self.device, config);
    }

    /// Open the device, selecting the adapter CEF produces its shared buffers
    /// on where that is knowable. `None` when this system has no usable GPU
    /// path at all.
    ///
    /// Creates the process's GPU instance, which on X11 must happen before the
    /// mpv proxy repoints `DISPLAY` and before mpv init: NVIDIA's Vulkan ICD
    /// does a lazy, one-time global init on first `vkCreateInstance` that
    /// includes an internal `XOpenDisplay`. Running it here keeps the ICD's
    /// connection on the real server and completes before mpv's VO thread is
    /// spawned, winning the loader-scan race that otherwise crashes NVIDIA
    /// proprietary (two threads reading a half-populated ICD dispatch table).
    pub fn init() -> Option<Self> {
        // Cheap pre-check: enumerating adapters needs no device and no surface,
        // so a machine with no GPU never pays for device creation.
        let producer = cef_render_node();
        let instance = build_instance();
        pick_adapter(&instance, producer)?;
        drop(instance);

        match Self::open(producer) {
            Ok(surfaces) => Some(surfaces),
            Err(e) => {
                tracing::info!("gpu_paint: device init failed: {e}");
                None
            }
        }
    }

    /// Whether *this device* can import CEF's shared buffers.
    ///
    /// The consumer half only. Whether CEF can *produce* them is a separate
    /// question, answered by `jfn_linux_util::dmabuf_probe`; callers AND the
    /// two to get the app-level answer CEF needs before any browser exists.
    pub fn can_import_shared(&self) -> bool {
        self.can_import_shared
    }

    fn open(producer: Option<(i64, i64)>) -> Result<Self, SurfaceLost> {
        let instance = build_instance();
        let (adapter, device_matched) = pick_adapter(&instance, producer).ok_or(Kind::NoAdapter)?;
        let info = adapter.get_info();
        let limits = adapter.limits();

        let (want_import, extra_exts) = unsafe {
            match adapter.as_hal::<vulkan::Api>() {
                Some(hal) => {
                    let ash_instance = hal.shared_instance().raw_instance();
                    let phys = hal.raw_physical_device();
                    (
                        shared_import::import_supported(ash_instance, phys),
                        shared_import::extra_device_extensions(ash_instance, phys),
                    )
                }
                None => (false, Vec::new()),
            }
        };

        let open_device = unsafe {
            let hal = adapter.as_hal::<vulkan::Api>().ok_or(Kind::NoAdapter)?;
            hal.open_with_callback(
                wgpu::Features::empty(),
                &limits,
                &wgpu::MemoryHints::Performance,
                Some(Box::new(move |args: vulkan::CreateDeviceCallbackArgs| {
                    for ext in &extra_exts {
                        if !args.extensions.contains(ext) {
                            args.extensions.push(*ext);
                        }
                    }
                })),
            )
            .map_err(|_| Kind::NoAdapter)?
        };

        let (device, queue) = unsafe {
            adapter.create_device_from_hal::<vulkan::Api>(
                open_device,
                &wgpu::DeviceDescriptor {
                    label: Some("jfn_gpu_paint device"),
                    required_features: wgpu::Features::empty(),
                    // Adapter limits — the swapchain may be larger than the
                    // downlevel 2048×2048 cap on modern displays.
                    required_limits: limits,
                    experimental_features: wgpu::ExperimentalFeatures::default(),
                    memory_hints: wgpu::MemoryHints::Performance,
                    trace: wgpu::Trace::Off,
                },
            )?
        };

        device.set_device_lost_callback(|reason, msg| {
            tracing::error!("gpu_paint: DEVICE LOST: {reason:?}: {msg}");
        });
        device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| {
            tracing::error!("gpu_paint: wgpu error: {e}");
        }));

        // Importing needs both halves: this device's extensions must be live,
        // and it must be the same device CEF allocates on — an import from a
        // different GPU fails at bind time.
        let can_import_shared =
            want_import && shared_import::required_extensions_enabled(&device) && device_matched;

        tracing::info!(
            adapter = %info.name,
            backend = ?info.backend,
            can_import_shared,
            "gpu_paint: device created"
        );

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            submit_gate: parking_lot::RwLock::new(()),
            can_import_shared,
        })
    }

    /// Bind a swapchain to one window. `size` seeds the swapchain extent; the
    /// surface takes its frame kind from the first frame presented to it.
    pub fn new_surface(
        &self,
        target: WindowTarget,
        size: PhysicalSize,
    ) -> Result<Surface<'_>, SurfaceLost> {
        Surface::new(self, target, size)
    }
}

/// The DRM render node CEF produces its shared buffers on, as `(major, minor)`.
///
/// The probe needs the ozone platform name, so ask the installed backend which
/// one we are on rather than guessing. On Wayland it gets a null EGL display
/// and yields `None`, which is what the caller passed explicitly before.
fn cef_render_node() -> Option<(i64, i64)> {
    let ozone = match jfn_platform_abi::try_get().map(|p| p.display()) {
        Some(jfn_platform_abi::DisplayBackend::Wayland) => c"wayland",
        _ => c"x11",
    };
    unsafe { jfn_linux_util::dmabuf_probe::cef_render_node(ozone.as_ptr(), std::ptr::null_mut()) }
}

fn build_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: native_backends(),
        flags: wgpu::InstanceFlags::empty(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

/// The one backend that can present on this platform. Kept to a single choice
/// so the adapter we probe is always the adapter we open.
const fn native_backends() -> wgpu::Backends {
    wgpu::Backends::VULKAN
}

/// Pick an adapter, and report whether it is the one CEF produces on. A
/// mismatch is not fatal — it only means shared import is unavailable.
fn pick_adapter(
    instance: &wgpu::Instance,
    producer: Option<(i64, i64)>,
) -> Option<(wgpu::Adapter, bool)> {
    let mut adapters: Vec<_> = pollster::block_on(instance.enumerate_adapters(native_backends()))
        .into_iter()
        .filter(|a| {
            !matches!(
                a.get_info().device_type,
                wgpu::DeviceType::Cpu | wgpu::DeviceType::Other
            )
        })
        .collect();

    if adapters.is_empty() {
        return None;
    }

    if let Some(want) = producer
        && let Some(pos) = adapters
            .iter()
            .position(|a| adapter_render_node(a) == Some(want))
    {
        return Some((adapters.swap_remove(pos), true));
    }

    let chosen = adapters
        .into_iter()
        .max_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 3,
            wgpu::DeviceType::IntegratedGpu => 2,
            wgpu::DeviceType::VirtualGpu => 1,
            _ => 0,
        })?;
    // With no device to match against, the best adapter is as good as it gets
    // and counts as matched; a device we asked for and missed does not.
    Some((chosen, producer.is_none()))
}

fn adapter_render_node(adapter: &wgpu::Adapter) -> Option<(i64, i64)> {
    unsafe { adapter.as_hal::<vulkan::Api>() }.and_then(|hal| {
        shared_import::drm_render_node(
            hal.shared_instance().raw_instance(),
            hal.raw_physical_device(),
        )
    })
}
