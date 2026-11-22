use crate::quad::QuadBlend;
use thinkterm_render::pipeline::Pipeline;
use anyhow::anyhow;
use config::{ConfigHandle, GpuInfo, WebGpuPowerPreference};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use window::bitmaps::Texture2d;
use window::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use window::{BitmapImage, Dimensions, Rect, Window, WindowOps};

fn gpu_debug_enabled() -> bool {
    std::env::var_os("THINKTERM_GPU_DEBUG").is_some()
}

fn gpu_debug(message: impl AsRef<str>) {
    if gpu_debug_enabled() {
        log::info!("[gpu-resource] {}", message.as_ref());
    }
}

pub use thinkterm_render::pipeline::ShaderUniform;

/// A persistent uniform buffer plus its bind group. The buffer is written
/// at most once per frame, so a slot can be reused every frame without
/// re-creating either object.
struct UniformSlot {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

#[derive(Default)]
struct FrameUniforms {
    window: Option<UniformSlot>,
    cards: Vec<UniformSlot>,
}

/// The two sampler flavours of the glyph atlas bind group, cached across
/// frames. `texture` is held strongly so its address can never be reused
/// while it serves as the identity key; when the atlas is recreated the
/// whole value is replaced and the old texture goes with it. Between the
/// recreation and the next completed draw this pins one stale atlas in
/// GPU memory — accepted, since a recreation is followed by a repaint of
/// the same window and the pin is bounded by a single atlas.
struct AtlasBindGroups {
    texture: Rc<dyn Texture2d>,
    linear: wgpu::BindGroup,
    nearest: wgpu::BindGroup,
}

pub struct WebGpuState {
    instance: wgpu::Instance,
    pub adapter_info: wgpu::AdapterInfo,
    pub downlevel_caps: wgpu::DownlevelCapabilities,
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: Arc<wgpu::Queue>,
    pub config: RefCell<wgpu::SurfaceConfiguration>,
    pub dimensions: RefCell<Dimensions>,
    /// The format drawn in but for a see-through window's frame: card
    /// pictures, the surfaces of other windows that show them, and the
    /// frame of an opaque window. The surface's sRGB one when it has one, so
    /// that colours mix in linear light.
    pub content_format: wgpu::TextureFormat,
    /// For `content_format`. A window's frame takes its pipelines from
    /// `frame_pipeline`.
    pub render_pipeline: wgpu::RenderPipeline,
    /// For quads that clear some of what is under them (`QuadBlend::Clear`).
    pub clear_pipeline: wgpu::RenderPipeline,
    /// For quads that add their light to it (`QuadBlend::Add`).
    pub add_pipeline: wgpu::RenderPipeline,
    /// For a see-through window's frame (`set_see_through`).
    see_through: Option<SeeThroughPipelines>,
    shader_uniform_bind_group_layout: wgpu::BindGroupLayout,
    pub texture_bind_group_layout: wgpu::BindGroupLayout,
    pub texture_nearest_sampler: wgpu::Sampler,
    pub texture_linear_sampler: wgpu::Sampler,
    frame_uniforms: RefCell<FrameUniforms>,
    atlas_bind_groups: RefCell<Option<AtlasBindGroups>>,
    /// A frame-latency change waiting to be folded into the next
    /// surface.configure. Reconfiguring drains the whole device queue on
    /// the calling thread, so it is never done eagerly on focus change;
    /// the next paint's resize() call picks it up.
    pending_frame_latency: Cell<Option<u32>>,
    /// Submission indices of the most recent frames, oldest first, so the
    /// paint path can bound how far the queue runs ahead of the device.
    ///
    /// Nothing else applies back-pressure: `queue.submit` never blocks, the
    /// swapchain's frame latency only bounds *presents*, and every frame's
    /// `write_buffer` calls mint staging buffers that wgpu cannot free until
    /// that frame's fence completes. On a device that cannot keep up (WARP,
    /// a busy integrated GPU) the backlog grew without limit: measured at
    /// ~460 MB/min of staging under three fast-refreshing btop panes, with
    /// the device up to 9 s behind. Waiting for the frame before last keeps
    /// at most two frames of uploads alive.
    in_flight_submissions: RefCell<std::collections::VecDeque<wgpu::SubmissionIndex>>,
    /// How many validation errors the device's error handler has seen.
    validation_errors: Arc<AtomicUsize>,
    /// Frames wgpu has turned down since the last one it took.
    rejected_frames: Cell<usize>,
    pub handle: RawHandlePair,
    /// Told of every present by WebGpuState::present.
    window: Window,
}

/// How many frames may be submitted ahead of the device before the paint
/// path blocks on the oldest one. Two matches `desired_maximum_frame_latency`.
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// The surface's format without its sRGB suffix, and the pipelines that draw
/// a see-through window's frame into it (`WebGpuState::set_see_through`).
struct SeeThroughPipelines {
    format: wgpu::TextureFormat,
    over: wgpu::RenderPipeline,
    clear: wgpu::RenderPipeline,
    add: wgpu::RenderPipeline,
}

/// `QuadBlend::Clear`: keeps `1 - alpha` of what is under the quad, colour
/// and solidity both, and adds nothing.
const CLEAR_BLENDING: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
        operation: wgpu::BlendOperation::Add,
    },
};

/// `QuadBlend::Add`: adds the quad's colour, times its alpha, and leaves how
/// solid what is under it is.
const ADD_BLENDING: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::SrcAlpha,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
    alpha: wgpu::BlendComponent {
        src_factor: wgpu::BlendFactor::Zero,
        dst_factor: wgpu::BlendFactor::One,
        operation: wgpu::BlendOperation::Add,
    },
};

pub struct RawHandlePair {
    window: RawWindowHandle,
    display: RawDisplayHandle,
}

impl RawHandlePair {
    fn new(window: &Window) -> Self {
        Self {
            window: window.window_handle().expect("window handle").as_raw(),
            display: window.display_handle().expect("display handle").as_raw(),
        }
    }

    fn surface_on(&self, instance: &wgpu::Instance) -> anyhow::Result<wgpu::Surface<'static>> {
        Ok(unsafe { instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(self)?)? })
    }
}

impl HasWindowHandle for RawHandlePair {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        unsafe { Ok(WindowHandle::borrow_raw(self.window)) }
    }
}

impl HasDisplayHandle for RawHandlePair {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        unsafe { Ok(DisplayHandle::borrow_raw(self.display)) }
    }
}

pub struct WebGpuTexture {
    texture: wgpu::Texture,
    width: u32,
    height: u32,
    queue: Arc<wgpu::Queue>,
}

impl std::ops::Deref for WebGpuTexture {
    type Target = wgpu::Texture;
    fn deref(&self) -> &Self::Target {
        &self.texture
    }
}

impl Texture2d for WebGpuTexture {
    fn write(&self, rect: Rect, im: &dyn BitmapImage) {
        let (im_width, im_height) = im.image_dimensions();

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.min_x() as u32,
                    y: rect.min_y() as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            im.pixel_data_slice(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(im_width as u32 * 4),
                rows_per_image: Some(im_height as u32),
            },
            wgpu::Extent3d {
                width: im_width as u32,
                height: im_height as u32,
                depth_or_array_layers: 1,
            },
        );
    }

    fn read(&self, _rect: Rect, _im: &mut dyn BitmapImage) {
        unimplemented!();
    }

    fn width(&self) -> usize {
        self.width as usize
    }

    fn height(&self) -> usize {
        self.height as usize
    }
}

impl WebGpuTexture {
    pub fn new(width: u32, height: u32, state: &WebGpuState) -> anyhow::Result<Self> {
        let limit = state.device.limits().max_texture_dimension_2d;

        if width > limit || height > limit {
            // Ideally, wgpu would have a fallible create_texture method,
            // but it doesn't: instead it will panic if the requested
            // dimension is too large.
            // So we check the limit ourselves here.
            // <https://github.com/wezterm/wezterm/issues/3713>
            anyhow::bail!(
                "texture dimensions {width}x{height} exceeed the \
                 max dimension {limit} supported by your GPU"
            );
        }

        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let view_formats = if state
            .downlevel_caps
            .flags
            .contains(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS)
        {
            vec![format, format.remove_srgb_suffix()]
        } else {
            vec![]
        };
        gpu_debug(format!(
            "create WebGpu texture label=Texture Atlas size={}x{} bytes={}",
            width,
            height,
            width as usize * height as usize * 4
        ));
        let texture = state.device.create_texture(&wgpu::TextureDescriptor {
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            label: Some("Texture Atlas"),
            view_formats: &view_formats,
        });
        Ok(Self {
            texture,
            width,
            height,
            queue: Arc::clone(&state.queue),
        })
    }
}

impl Drop for WebGpuTexture {
    fn drop(&mut self) {
        gpu_debug(format!(
            "drop WebGpu texture label=Texture Atlas size={}x{} bytes={}",
            self.width,
            self.height,
            self.width as usize * self.height as usize * 4
        ));
    }
}

/// An overview card's private render target. The card's quads are rendered
/// into this once per content change; every other frame composites it as a
/// single textured quad, instead of re-emitting and re-uploading the card's
/// thousands of glyph quads.
pub struct CardRenderTexture {
    pub texture: wgpu::Texture,
    pub view: wgpu::TextureView,
    /// Card texture + linear sampler, bindable in either of the pipeline's
    /// two texture slots (their layouts are identical).
    pub bind_group: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
}

impl CardRenderTexture {
    pub fn new(width: u32, height: u32, state: &WebGpuState) -> anyhow::Result<Self> {
        Self::with_copies(width, height, state, crate::framedump::enabled())
    }

    /// One that can be copied onto a surface rather than only drawn.
    pub fn copyable(width: u32, height: u32, state: &WebGpuState) -> anyhow::Result<Self> {
        Self::with_copies(width, height, state, true)
    }

    fn with_copies(
        width: u32,
        height: u32,
        state: &WebGpuState,
        copies: bool,
    ) -> anyhow::Result<Self> {
        let limit = state.device.limits().max_texture_dimension_2d;
        if width > limit || height > limit {
            anyhow::bail!(
                "card texture dimensions {width}x{height} exceed the \
                 max dimension {limit} supported by your GPU"
            );
        }
        // The format the shared render pipeline targets, which another
        // window's surface it is copied onto has too.
        let format = state.content_format;
        let texture = state.device.create_texture(&wgpu::TextureDescriptor {
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: if copies {
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
            },
            label: Some("Card Texture"),
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = state.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &state.texture_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&state.texture_linear_sampler),
                },
            ],
            label: Some("card texture bind group"),
        });
        Ok(Self {
            texture,
            view,
            bind_group,
            width,
            height,
        })
    }
}

/// A dedicated texture for one large image -- a streaming kitty frame, an
/// `icat` picture -- drawn as its own composite instead of being packed into
/// the shared glyph atlas. The atlas cannot free a rectangle, so a stream of
/// multi-megabyte frames filled it every few frames and forced a full clear
/// (a 64MiB zero image, a 64MiB upload and a second paint pass) each time.
/// These textures are reused across frames of the same size, so a stream
/// costs one `write_texture` per frame and nothing else.
pub struct ImageTexture {
    pub texture: wgpu::Texture,
    /// Same layout as the atlas bind groups, so the composite pass binds
    /// linear into slot 1 and nearest into slot 2 exactly like the main pass
    /// does for the atlas, and the shader's minification mix behaves the
    /// same on both paths (a single linear binding would soften 1:1 output).
    pub bind_group_linear: wgpu::BindGroup,
    pub bind_group_nearest: wgpu::BindGroup,
    pub width: u32,
    pub height: u32,
    queue: Arc<wgpu::Queue>,
}

impl ImageTexture {
    pub fn new(width: u32, height: u32, state: &WebGpuState) -> anyhow::Result<Self> {
        let limit = state.device.limits().max_texture_dimension_2d;
        if width == 0 || height == 0 || width > limit || height > limit {
            anyhow::bail!(
                "image texture dimensions {width}x{height} are outside the \
                 1..={limit} range supported by your GPU"
            );
        }
        gpu_debug(format!(
            "create WebGpu texture label=Image Texture size={width}x{height} bytes={}",
            width as usize * height as usize * 4
        ));
        // The same format as the atlas: the pixels are the same RGBA8 bytes
        // the atlas path uploads today, just not through the allocator.
        let texture = state.device.create_texture(&wgpu::TextureDescriptor {
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            label: Some("Image Texture"),
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let make = |sampler: &wgpu::Sampler, label: &str| {
            state.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &state.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
                label: Some(label),
            })
        };
        let bind_group_linear = make(&state.texture_linear_sampler, "image texture linear");
        let bind_group_nearest = make(&state.texture_nearest_sampler, "image texture nearest");
        Ok(Self {
            texture,
            bind_group_linear,
            bind_group_nearest,
            width,
            height,
            queue: Arc::clone(&state.queue),
        })
    }

    /// Upload tightly packed RGBA8 pixels covering the whole texture.
    pub fn upload(&self, rgba: &[u8]) {
        debug_assert_eq!(rgba.len(), self.byte_size());
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(self.width * 4),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn byte_size(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

pub fn adapter_info_to_gpu_info(info: wgpu::AdapterInfo) -> GpuInfo {
    GpuInfo {
        name: info.name,
        vendor: Some(info.vendor),
        device: Some(info.device),
        device_type: format!("{:?}", info.device_type),
        driver: if info.driver.is_empty() {
            None
        } else {
            Some(info.driver)
        },
        driver_info: if info.driver_info.is_empty() {
            None
        } else {
            Some(info.driver_info)
        },
        backend: format!("{:?}", info.backend),
    }
}

/// The backends a WebGpu instance asks for. Where OpenGL would be Mesa -- a
/// remote session, or `software` (front_end = "Software") -- Windows leaves
/// GL out: there it is WGL on opengl32.dll, and once anything has loaded that
/// DLL the OpenGL renderer can no longer reach the Mesa copy (see
/// wezterm-gui/build.rs). DX12 and Vulkan still cover WebGpu in those
/// sessions. Reads no configuration itself; the caller passes `software`.
pub fn instance_backends(software: bool) -> wgpu::Backends {
    if cfg!(windows) && (software || window::in_remote_session()) {
        wgpu::Backends::all().difference(wgpu::Backends::GL)
    } else {
        wgpu::Backends::all()
    }
}

fn compute_compatibility_list(
    instance: &wgpu::Instance,
    backends: wgpu::Backends,
    surface: &wgpu::Surface,
) -> Vec<String> {
    instance
        .enumerate_adapters(backends)
        .into_iter()
        .map(|a| {
            let info = adapter_info_to_gpu_info(a.get_info());
            let compatible = a.is_surface_supported(&surface);
            format!(
                "{}, compatible={}",
                info.to_string(),
                if compatible { "yes" } else { "NO" }
            )
        })
        .collect()
}

impl WebGpuState {
    pub async fn new(
        window: &Window,
        dimensions: Dimensions,
        config: &ConfigHandle,
    ) -> anyhow::Result<Self> {
        Self::new_impl(window.clone(), dimensions, config).await
    }

    pub async fn new_impl(
        window: Window,
        dimensions: Dimensions,
        config: &ConfigHandle,
    ) -> anyhow::Result<Self> {
        let handle = RawHandlePair::new(&window);
        let backends = instance_backends(config.front_end == config::FrontEndSelection::Software);
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        let surface = handle.surface_on(&instance)?;

        let mut adapter: Option<wgpu::Adapter> = None;

        if let Some(preference) = &config.webgpu_preferred_adapter {
            for a in instance.enumerate_adapters(backends) {
                if !a.is_surface_supported(&surface) {
                    let info = adapter_info_to_gpu_info(a.get_info());
                    log::warn!("{} is not compatible with surface", info.to_string());
                    continue;
                }

                let info = a.get_info();

                if preference.name != info.name {
                    continue;
                }

                if preference.device_type != format!("{:?}", info.device_type) {
                    continue;
                }

                if preference.backend != format!("{:?}", info.backend) {
                    continue;
                }

                if let Some(driver) = &preference.driver {
                    if *driver != info.driver {
                        continue;
                    }
                }
                if let Some(vendor) = &preference.vendor {
                    if *vendor != info.vendor {
                        continue;
                    }
                }
                if let Some(device) = &preference.device {
                    if *device != info.device {
                        continue;
                    }
                }

                adapter.replace(a);
                break;
            }

            if adapter.is_none() {
                let adapters = compute_compatibility_list(&instance, backends, &surface);
                log::warn!(
                    "Your webgpu preferred adapter '{}' was either not \
                     found or is not compatible with your display. Available:\n{}",
                    preference.to_string(),
                    adapters.join("\n")
                );
            }
        }

        if adapter.is_none() {
            adapter = Some(
                instance
                    .request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: match config.webgpu_power_preference {
                            WebGpuPowerPreference::HighPerformance => {
                                wgpu::PowerPreference::HighPerformance
                            }
                            WebGpuPowerPreference::LowPower => wgpu::PowerPreference::LowPower,
                        },
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: config.webgpu_force_fallback_adapter,
                    })
                    .await?,
            );
        }

        let adapter = adapter.ok_or_else(|| {
            let adapters = compute_compatibility_list(&instance, backends, &surface);
            anyhow!(
                "no compatible adapter found. Available:\n{}",
                adapters.join("\n")
            )
        })?;

        // wgpu's LowPower preference (the default) picks an integrated GPU.
        // On a hybrid system where the panel is wired to the discrete GPU,
        // the integrated adapter renders frames that the compositor never
        // presents, leaving a black window. Prefer an adapter that drives a
        // connected display when the one wgpu picked drives none. An explicit
        // webgpu_preferred_adapter is always honoured, and so is an explicit
        // HighPerformance: on the usual hybrid laptop the panel hangs off the
        // integrated GPU, and the discrete one that person asked for drives
        // no display of its own.
        #[cfg(target_os = "linux")]
        let adapter = {
            let mut adapter = adapter;
            if config.front_end == config::FrontEndSelection::WebGpu
                && config.webgpu_preferred_adapter.is_none()
                && !config.webgpu_force_fallback_adapter
                && config.webgpu_power_preference == WebGpuPowerPreference::LowPower
            {
                crate::termwindow::display_gpu::prefer_display_gpu(
                    &instance,
                    backends,
                    &surface,
                    &mut adapter,
                );
            }
            adapter
        };

        let adapter_info = adapter.get_info();
        log::trace!("Using adapter: {adapter_info:?}");
        let caps = surface.get_capabilities(&adapter);
        log::trace!("caps: {caps:?}");
        let downlevel_caps = adapter.get_downlevel_capabilities();
        log::trace!("downlevel_caps: {downlevel_caps:?}");

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_features: wgpu::Features::empty(),
                // WebGL doesn't support all of wgpu's features, so if
                // we're building for the web we'll have to disable some.
                required_limits: if cfg!(target_arch = "wasm32") {
                    wgpu::Limits::downlevel_webgl2_defaults()
                } else {
                    wgpu::Limits::downlevel_defaults()
                }
                .using_resolution(adapter.limits()),
                label: None,
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                trace: wgpu::Trace::Off,
            })
            .await?;

        // wgpu's default reaction to an uncaptured error is to abort the
        // process. A validation error during a paint is recoverable — at
        // worst one garbled frame — and this has killed the app repeatedly
        // under video-rate kitty streams, so log it (its text names the
        // actual offender) and keep running. OutOfMemory/Internal are a
        // different story: the device is in no state to keep rendering, and
        // limping on would just be a silent blank window — die loudly.
        // Validation errors are counted for `submit_frame`.
        let validation_errors = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&validation_errors);
        device.on_uncaptured_error(Box::new(move |err| match &err {
            wgpu::Error::Validation { .. } => {
                counted.fetch_add(1, Ordering::Relaxed);
                log::error!("wgpu validation error (continuing): {err}");
            }
            _ => {
                panic!("fatal wgpu error: {}", err);
            }
        }));

        let queue = Arc::new(queue);

        // Explicitly request an SRGB format, if available
        let pref_format_srgb = caps.formats[0].add_srgb_suffix();
        let format = if caps.formats.contains(&pref_format_srgb) {
            pref_format_srgb
        } else {
            caps.formats[0]
        };

        // Need to check that this is supported, as trying to set
        // view_formats without it will cause surface.configure
        // to panic
        // <https://github.com/wezterm/wezterm/issues/3565>
        let view_formats = if downlevel_caps
            .flags
            .contains(wgpu::DownlevelFlags::SURFACE_VIEW_FORMATS)
        {
            vec![format.add_srgb_suffix(), format.remove_srgb_suffix()]
        } else {
            vec![]
        };

        let config = wgpu::SurfaceConfiguration {
            usage: if crate::framedump::enabled() {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT
            },
            format,
            width: dimensions.pixel_width as u32,
            height: dimensions.pixel_height as u32,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: if caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PostMultiplied)
            {
                wgpu::CompositeAlphaMode::PostMultiplied
            } else if caps
                .alpha_modes
                .contains(&wgpu::CompositeAlphaMode::PreMultiplied)
            {
                wgpu::CompositeAlphaMode::PreMultiplied
            } else {
                wgpu::CompositeAlphaMode::Auto
            },
            view_formats,
            desired_maximum_frame_latency: 2,
        };
        gpu_debug(format!(
            "configure WebGpu surface initial size={}x{} format={:?} frame_latency={}",
            config.width, config.height, config.format, config.desired_maximum_frame_latency
        ));
        surface.configure(&device, &config);

        // The shader, layouts, samplers and pipeline are the shared
        // renderer's; the browser builds the very same ones.
        let (pipeline, blended) =
            Pipeline::with_blends(&device, config.format, &[CLEAR_BLENDING, ADD_BLENDING]);
        let mut blended = blended.into_iter();
        let (Some(clear_pipeline), Some(add_pipeline)) = (blended.next(), blended.next()) else {
            anyhow::bail!("no pipeline for each blend");
        };
        let plain = format.remove_srgb_suffix();
        let see_through = if format.is_srgb() && caps.formats.contains(&plain) {
            let mut built = pipeline
                .pipelines_for(
                    &device,
                    plain,
                    true,
                    &[
                        wgpu::BlendState::ALPHA_BLENDING,
                        CLEAR_BLENDING,
                        ADD_BLENDING,
                    ],
                )
                .into_iter();
            let (Some(over), Some(clear), Some(add)) = (built.next(), built.next(), built.next())
            else {
                anyhow::bail!("no see-through pipeline for each blend");
            };
            Some(SeeThroughPipelines {
                format: plain,
                over,
                clear,
                add,
            })
        } else {
            None
        };

        Ok(Self {
            instance,
            adapter_info,
            downlevel_caps,
            surface,
            device,
            queue,
            config: RefCell::new(config),
            dimensions: RefCell::new(dimensions),
            content_format: format,
            render_pipeline: pipeline.render_pipeline,
            clear_pipeline,
            add_pipeline,
            see_through,
            handle,
            window,
            shader_uniform_bind_group_layout: pipeline.uniform_layout,
            texture_bind_group_layout: pipeline.texture_layout,
            texture_nearest_sampler: pipeline.nearest_sampler,
            texture_linear_sampler: pipeline.linear_sampler,
            frame_uniforms: RefCell::new(FrameUniforms::default()),
            atlas_bind_groups: RefCell::new(None),
            pending_frame_latency: Cell::new(None),
            in_flight_submissions: RefCell::new(std::collections::VecDeque::with_capacity(
                MAX_FRAMES_IN_FLIGHT + 1,
            )),
            validation_errors,
            rejected_frames: Cell::new(0),
        })
    }

    /// A surface on this device for another window of ours, which can then
    /// show what this one has rendered without a device of its own. The
    /// handle goes with the surface.
    pub fn surface_for(
        &self,
        window: &Window,
        width: u32,
        height: u32,
    ) -> anyhow::Result<(wgpu::Surface<'static>, RawHandlePair)> {
        let handle = RawHandlePair::new(window);
        let surface = handle.surface_on(&self.instance)?;
        let mut config = self.config.borrow().clone();
        config.format = self.content_format;
        config.width = width;
        config.height = height;
        config.usage |= wgpu::TextureUsages::COPY_DST;
        surface.configure(&self.device, &config);
        Ok((surface, handle))
    }

    /// Present a frame and tell the window, which wgpu does not: on Wayland
    /// the window paces its repaints by it (WindowOps::frame_presented).
    pub fn present(&self, output: wgpu::SurfaceTexture) {
        output.present();
        self.window.frame_presented();
    }

    /// Block until the device has finished the oldest frame still in flight,
    /// when `MAX_FRAMES_IN_FLIGHT` frames are already queued ahead of it.
    /// Call once per frame, before the frame's first `write_buffer`, so the
    /// backlog of upload staging never exceeds that many frames.
    pub fn wait_for_frame_slot(&self) {
        let oldest = {
            let mut queue = self.in_flight_submissions.borrow_mut();
            if queue.len() < MAX_FRAMES_IN_FLIGHT {
                return;
            }
            queue.pop_front()
        };
        if let Some(index) = oldest {
            let started = crate::perf::now();
            if let Err(err) = self
                .device
                .poll(wgpu::PollType::WaitForSubmissionIndex(index))
            {
                log::warn!("waiting for an in-flight frame submission: {err:?}");
            }
            crate::perf::log_duration("webgpu_frame_slot_wait", started);
        }
    }

    /// Submit a frame's commands, and keep its index for
    /// `wait_for_frame_slot` unless wgpu turned the frame down.
    ///
    /// A frame that fails validation, in its commands or in the submit, is
    /// not submitted, yet `Queue::submit` still hands back an index for it,
    /// one the device never reaches. To `Device::poll` a wait on that index
    /// is `WaitIdleError::WrongSubmissionIndex`, which it does not return
    /// but panics on, and a panic in a paint callback aborts the app: two
    /// frames turned down in a row did that. wgpu reports an error to the
    /// device's handler as the call that made it returns, so a count that
    /// moved over `finish` and `submit` is this frame's, or else one from
    /// the callbacks `submit` runs for wgpu itself: a frame that did go out
    /// then only goes without its wait.
    pub fn submit_frame(&self, encoder: wgpu::CommandEncoder) {
        let errors = self.validation_errors.load(Ordering::Relaxed);
        let index = self.queue.submit(std::iter::once(encoder.finish()));
        if self.validation_errors.load(Ordering::Relaxed) == errors {
            let rejected = self.rejected_frames.replace(0);
            if rejected > 0 {
                log::warn!("wgpu turned down {rejected} frames in a row");
            }
            self.in_flight_submissions.borrow_mut().push_back(index);
            return;
        }
        if self.rejected_frames.replace(self.rejected_frames.get() + 1) == 0 {
            log::warn!("wgpu turned a frame down; it is not waited for");
        }
        // The frame's uploads are still queued: wgpu sends them with the
        // next submission that goes out, and while frames keep being turned
        // down they pile up at the frame rate. A submission of nothing sends
        // them, and is a frame of its own to wait for.
        let errors = self.validation_errors.load(Ordering::Relaxed);
        let flushed = self.queue.submit(std::iter::empty());
        if self.validation_errors.load(Ordering::Relaxed) == errors {
            self.in_flight_submissions.borrow_mut().push_back(flushed);
        }
    }

    /// Ask for a different swapchain frame latency. Applied lazily: the
    /// next paint's resize() folds it into the surface.configure it
    /// already makes, so the blocking queue drain is never paid twice.
    pub fn set_desired_frame_latency(&self, latency: u32) {
        if self.config.borrow().desired_maximum_frame_latency == latency {
            self.pending_frame_latency.set(None);
        } else {
            self.pending_frame_latency.set(Some(latency));
        }
    }

    /// The atlas texture only changes identity when the glyph cache is
    /// rebuilt (atlas growth, DPI/font change), so the two sampler bind
    /// groups are cached against it and rebuilt on identity change alone.
    pub fn atlas_bind_groups(
        &self,
        texture: &Rc<dyn Texture2d>,
    ) -> (wgpu::BindGroup, wgpu::BindGroup) {
        let mut cache = self.atlas_bind_groups.borrow_mut();
        if let Some(cached) = cache.as_ref() {
            if Rc::ptr_eq(&cached.texture, texture) {
                return (cached.linear.clone(), cached.nearest.clone());
            }
        }
        gpu_debug("rebuild atlas bind groups (atlas texture changed)");
        let tex = texture
            .downcast_ref::<WebGpuTexture>()
            .expect("webgpu render path holds a WebGpuTexture atlas");
        let texture_view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let make = |sampler: &wgpu::Sampler, label: &str| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.texture_bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&texture_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
                label: Some(label),
            })
        };
        let linear = make(&self.texture_linear_sampler, "linear bind group");
        let nearest = make(&self.texture_nearest_sampler, "nearest bind group");
        *cache = Some(AtlasBindGroups {
            texture: Rc::clone(texture),
            linear: linear.clone(),
            nearest: nearest.clone(),
        });
        (linear, nearest)
    }

    fn make_uniform_slot(&self) -> UniformSlot {
        crate::perf::log_counter("uniform_slot_creates", 1);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ShaderUniform Buffer"),
            size: std::mem::size_of::<ShaderUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.shader_uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
            label: Some("ShaderUniform Bind Group"),
        });
        UniformSlot { buffer, bind_group }
    }

    /// The whole-window uniform value; every non-card pass in a frame uses
    /// the same value, so they all share this one slot.
    pub fn window_uniform(&self, uniform: ShaderUniform) -> wgpu::BindGroup {
        let mut frame = self.frame_uniforms.borrow_mut();
        let slot = match &frame.window {
            Some(slot) => slot,
            None => {
                frame.window = Some(self.make_uniform_slot());
                frame.window.as_ref().unwrap()
            }
        };
        self.queue
            .write_buffer(&slot.buffer, 0, bytemuck::bytes_of(&uniform));
        slot.bind_group.clone()
    }

    /// Per-card uniform slots. Each slot's buffer may only be written once
    /// per frame (queued writes all land at the head of the submit), which
    /// holds because each pending card gets its own slot index.
    pub fn card_uniform(&self, slot: usize, uniform: ShaderUniform) -> wgpu::BindGroup {
        let mut frame = self.frame_uniforms.borrow_mut();
        while frame.cards.len() <= slot {
            let new_slot = self.make_uniform_slot();
            frame.cards.push(new_slot);
        }
        let slot = &frame.cards[slot];
        self.queue
            .write_buffer(&slot.buffer, 0, bytemuck::bytes_of(&uniform));
        slot.bind_group.clone()
    }

    /// Draw the window's frame for a window `see_through` or not.
    ///
    /// The system lays a see-through window's frame over what is behind it
    /// as its sRGB colour times its alpha. Mixed in linear light, as an
    /// opaque frame is, a light colour leaves more than that: white at 70% is
    /// stored as 218 where 70% of 255 is 178, so over all but a dark desktop
    /// a light window comes out white. A dark one is all but unchanged. So a
    /// see-through window's frame is drawn into the surface's format without
    /// the sRGB suffix, the shader writing sRGB values that then mix as they
    /// are, as the OpenGL renderer does (`fs_main_srgb`). A surface without
    /// that format is left as it is.
    pub fn set_see_through(&self, see_through: bool) {
        let format = match &self.see_through {
            Some(pipelines) if see_through => pipelines.format,
            _ => self.content_format,
        };
        let mut config = self.config.borrow_mut();
        if config.format == format {
            return;
        }
        gpu_debug(format!(
            "WebGpu surface format {:?} -> {:?} see_through={}",
            config.format, format, see_through
        ));
        config.format = format;
        // A zero-sized surface takes it with its next resize.
        if config.width > 0 && config.height > 0 {
            self.surface.configure(&self.device, &config);
        }
    }

    /// The pipeline the window's frame lays a quad mixed as `blend` with:
    /// for the format `set_see_through` gave the surface.
    pub fn frame_pipeline(&self, blend: QuadBlend) -> &wgpu::RenderPipeline {
        match &self.see_through {
            Some(pipelines) if self.config.borrow().format == pipelines.format => match blend {
                QuadBlend::Over => &pipelines.over,
                QuadBlend::Clear => &pipelines.clear,
                QuadBlend::Add => &pipelines.add,
            },
            _ => match blend {
                QuadBlend::Over => &self.render_pipeline,
                QuadBlend::Clear => &self.clear_pipeline,
                QuadBlend::Add => &self.add_pipeline,
            },
        }
    }

    #[allow(unused_mut)]
    pub fn resize(&self, mut dims: Dimensions) {
        // During a live resize on Windows, the Dimensions that we're processing may be
        // lagging behind the true client size. We have to take the very latest value
        // from the window or else the underlying driver will raise an error about
        // the mismatch, so we need to sneakily read through the handle
        match self.handle.window {
            #[cfg(windows)]
            RawWindowHandle::Win32(h) => {
                let mut rect = unsafe { std::mem::zeroed() };
                unsafe { winapi::um::winuser::GetClientRect(h.hwnd.get() as _, &mut rect) };
                dims.pixel_width = (rect.right - rect.left) as usize;
                dims.pixel_height = (rect.bottom - rect.top) as usize;
            }
            _ => {}
        }

        let pending_latency = self.pending_frame_latency.get();
        if dims == *self.dimensions.borrow() && pending_latency.is_none() {
            return;
        }
        let old = *self.dimensions.borrow();
        *self.dimensions.borrow_mut() = dims;
        let mut config = self.config.borrow_mut();
        config.width = dims.pixel_width as u32;
        config.height = dims.pixel_height as u32;
        if config.width > 0 && config.height > 0 {
            // The latency lands in the config only alongside the configure
            // that applies it: writing it on the skipped zero-size path
            // would desync the config from the real surface and make
            // set_desired_frame_latency's equality check swallow a change
            // that never took effect.
            if let Some(latency) = pending_latency {
                config.desired_maximum_frame_latency = latency;
            }
            gpu_debug(format!(
                "resize WebGpu surface {}x{} -> {}x{} frame_latency={}",
                old.pixel_width,
                old.pixel_height,
                config.width,
                config.height,
                config.desired_maximum_frame_latency
            ));
            // Avoid reconfiguring with a 0 sized surface, as webgpu will
            // panic in that case
            // <https://github.com/wezterm/wezterm/issues/2881>
            self.surface.configure(&self.device, &config);
            // Consumed only by a real configure: a zero-sized (minimized)
            // surface must keep the change pending for later.
            self.pending_frame_latency.set(None);
        }
    }
}

impl Drop for WebGpuState {
    fn drop(&mut self) {
        let dims = *self.dimensions.borrow();
        gpu_debug(format!(
            "drop WebGpu state surface size={}x{}",
            dims.pixel_width, dims.pixel_height
        ));
    }
}
