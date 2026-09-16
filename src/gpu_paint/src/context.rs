use crate::error::{Kind, SurfaceLost};
use crate::painter::{AlphaSource, Surface};
use crate::shared;
use crate::swapchain::Swapchain;
use crate::types::WindowTarget;
use crate::{FrameSize, ProducerId};

pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

pub struct Surfaces {
    pub(crate) instance: wgpu::Instance,
    pub(crate) adapter: wgpu::Adapter,
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) bind_layout: wgpu::BindGroupLayout,
    pub(crate) sampler: wgpu::Sampler,
    pub(crate) max_texture_dim: u32,
    pipeline: wgpu::RenderPipeline,
    pipeline_premultiplied: wgpu::RenderPipeline,
    pub(crate) submit_gate: parking_lot::RwLock<()>,
    can_import_shared: bool,
}

static INSTANCE: std::sync::OnceLock<Option<Surfaces>> = std::sync::OnceLock::new();

pub fn surfaces() -> Option<&'static Surfaces> {
    INSTANCE.get()?.as_ref()
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

    pub(crate) fn pipeline(&self, alpha: AlphaSource) -> &wgpu::RenderPipeline {
        match alpha {
            AlphaSource::Premultiplied => &self.pipeline,
            AlphaSource::Straight => &self.pipeline_premultiplied,
        }
    }

    pub fn init(producer: Option<ProducerId>) -> Option<&'static Surfaces> {
        INSTANCE
            .get_or_init(|| match Self::open(producer) {
                Ok(surfaces) => Some(surfaces),
                Err(e) => {
                    tracing::info!("gpu_paint: device init failed: {e}");
                    None
                }
            })
            .as_ref()
    }

    pub fn can_import_shared(&self) -> bool {
        self.can_import_shared
    }

    fn open(producer: Option<shared::ProducerId>) -> Result<Self, SurfaceLost> {
        let (adapter, device_matched) = pick_adapter(producer).ok_or(Kind::NoAdapter)?;
        let instance = enumerated().instance.clone();
        let info = adapter.get_info();

        let opened = shared::open_device(&adapter)?;
        let shared::Opened {
            device,
            queue,
            import_capable,
        } = opened;

        device.set_device_lost_callback(|reason, msg| {
            tracing::error!("gpu_paint: DEVICE LOST: {reason:?}: {msg}");
        });
        device.on_uncaptured_error(std::sync::Arc::new(|e: wgpu::Error| {
            tracing::error!("gpu_paint: wgpu error: {e}");
        }));

        let can_import_shared = import_capable && device_matched;

        tracing::info!(
            "gpu_paint: device created on {} ({:?}), can_import_shared={can_import_shared} (device_matched={device_matched})",
            info.name,
            info.backend,
        );

        let Pipelines {
            bind_layout,
            sampler,
            pipeline,
            pipeline_premultiplied,
        } = build_pipelines(&device);

        let max_texture_dim = device.limits().max_texture_dimension_2d;

        Ok(Self {
            instance,
            adapter,
            device,
            queue,
            bind_layout,
            sampler,
            max_texture_dim,
            pipeline,
            pipeline_premultiplied,
            submit_gate: parking_lot::RwLock::new(()),
            can_import_shared,
        })
    }

    pub fn new_surface(
        &self,
        target: WindowTarget,
        size: FrameSize,
    ) -> Result<Surface<'_>, SurfaceLost> {
        Surface::new(self, target, size)
    }

    pub fn new_swapchain(
        &self,
        target: WindowTarget,
        size: FrameSize,
    ) -> Result<Swapchain<'_>, SurfaceLost> {
        Swapchain::new(self, target, size)
    }

    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    #[cfg(windows)]
    pub fn adapter_luid(&self) -> Option<ProducerId> {
        shared::adapter_luid(&self.adapter)
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
}

pub fn any_adapter() -> bool {
    !enumerated().adapters.is_empty()
}

struct Pipelines {
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    pipeline: wgpu::RenderPipeline,
    pipeline_premultiplied: wgpu::RenderPipeline,
}

fn build_pipelines(device: &wgpu::Device) -> Pipelines {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("jfn_gpu_paint overlay"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/overlay.wgsl").into()),
    });

    let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("jfn_gpu_paint bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("jfn_gpu_paint pl"),
        bind_group_layouts: &[Some(&bind_layout)],
        immediate_size: 0,
    });

    let pipeline = build_pipeline(device, &pipeline_layout, &shader, "fs_main");
    let pipeline_premultiplied =
        build_pipeline(device, &pipeline_layout, &shader, "fs_main_premultiplied");

    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("jfn_gpu_paint sampler"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        address_mode_w: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        mipmap_filter: wgpu::MipmapFilterMode::Nearest,
        ..Default::default()
    });

    Pipelines {
        bind_layout,
        sampler,
        pipeline,
        pipeline_premultiplied,
    }
}

fn build_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    fragment_entry: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("jfn_gpu_paint pipeline"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

fn build_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: native_backends(),
        flags: wgpu::InstanceFlags::empty(),
        backend_options: instance_options(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

const fn native_backends() -> wgpu::Backends {
    #[cfg(target_os = "linux")]
    {
        wgpu::Backends::VULKAN
    }
    #[cfg(windows)]
    {
        wgpu::Backends::DX12
    }
    #[cfg(target_os = "macos")]
    {
        wgpu::Backends::METAL
    }
}

fn instance_options() -> wgpu::BackendOptions {
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut options = wgpu::BackendOptions::default();
    #[cfg(windows)]
    {
        options.dx12.latency_waitable_object = wgpu::Dx12UseFrameLatencyWaitableObject::None;
    }
    options
}

struct Enumerated {
    instance: wgpu::Instance,
    adapters: Vec<wgpu::Adapter>,
}

static ENUMERATED: std::sync::OnceLock<Enumerated> = std::sync::OnceLock::new();

fn enumerated() -> &'static Enumerated {
    ENUMERATED.get_or_init(|| {
        let instance = build_instance();
        let adapters = pollster::block_on(instance.enumerate_adapters(native_backends()))
            .into_iter()
            .filter(|a| {
                !matches!(
                    a.get_info().device_type,
                    wgpu::DeviceType::Cpu | wgpu::DeviceType::Other
                )
            })
            .collect();
        Enumerated { instance, adapters }
    })
}

fn pick_adapter(producer: Option<shared::ProducerId>) -> Option<(wgpu::Adapter, bool)> {
    let adapters = &enumerated().adapters;

    if let Some(want) = producer
        && let Some(found) = adapters.iter().find(|a| shared::adapter_matches(a, want))
    {
        return Some((found.clone(), true));
    }

    let chosen = adapters
        .iter()
        .max_by_key(|a| match a.get_info().device_type {
            wgpu::DeviceType::DiscreteGpu => 3,
            wgpu::DeviceType::IntegratedGpu => 2,
            wgpu::DeviceType::VirtualGpu => 1,
            _ => 0,
        })?;
    Some((chosen.clone(), producer.is_none()))
}
