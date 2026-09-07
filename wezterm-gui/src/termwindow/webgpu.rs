use thinkterm_render::pipeline::Pipeline;
use anyhow::anyhow;
use config::{ConfigHandle, GpuInfo, WebGpuPowerPreference};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use window::bitmaps::Texture2d;
use window::raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use window::{BitmapImage, Dimensions, Rect, Window};

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
    pub adapter_info: wgpu::AdapterInfo,
    pub downlevel_caps: wgpu::DownlevelCapabilities,
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: Arc<wgpu::Queue>,
    pub config: RefCell<wgpu::SurfaceConfiguration>,
    pub dimensions: RefCell<Dimensions>,
    pub render_pipeline: wgpu::RenderPipeline,
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
    pub handle: RawHandlePair,
}

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
        let limit = state.device.limits().max_texture_dimension_2d;
        if width > limit || height > limit {
            anyhow::bail!(
                "card texture dimensions {width}x{height} exceed the \
                 max dimension {limit} supported by your GPU"
            );
        }
        // Same format as the surface so the shared render pipeline can
        // target it.
        let format = state.config.borrow().format;
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
            usage: if crate::framedump::enabled() {
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
        let handle = RawHandlePair::new(window);
        Self::new_impl(handle, dimensions, config).await
    }

    pub async fn new_impl(
        handle: RawHandlePair,
        dimensions: Dimensions,
        config: &ConfigHandle,
    ) -> anyhow::Result<Self> {
        let backends = wgpu::Backends::all();
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends,
            ..Default::default()
        });
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::from_window(&handle)?)?
        };

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
                memory_hints: Default::default(),
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
        device.on_uncaptured_error(Box::new(|err| match &err {
            wgpu::Error::Validation { .. } => {
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
        let pipeline = Pipeline::new(&device, config.format);

        Ok(Self {
            adapter_info,
            downlevel_caps,
            surface,
            device,
            queue,
            config: RefCell::new(config),
            dimensions: RefCell::new(dimensions),
            render_pipeline: pipeline.render_pipeline,
            handle,
            shader_uniform_bind_group_layout: pipeline.uniform_layout,
            texture_bind_group_layout: pipeline.texture_layout,
            texture_nearest_sampler: pipeline.nearest_sampler,
            texture_linear_sampler: pipeline.linear_sampler,
            frame_uniforms: RefCell::new(FrameUniforms::default()),
            atlas_bind_groups: RefCell::new(None),
            pending_frame_latency: Cell::new(None),
        })
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
