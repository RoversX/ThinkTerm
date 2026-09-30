//! The canvas surface and one frame's worth of drawing through the shared
//! pipeline. Colour: the canvas is configured with the linear name of its
//! format and drawn through an sRGB view, so the hardware encodes on write
//! exactly as it does for the desktop's already-sRGB surface.

use anyhow::{anyhow, Result};
use std::rc::Rc;
use std::sync::Arc;
use thinkterm_render::pipeline::{
    centred_projection, quad_indices, srgb_format, view_as, AtlasBindGroups, GpuTexture, Pipeline,
    ShaderUniform,
};
use thinkterm_render::vertex::Vertex;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: Arc<wgpu::Queue>,
    /// The surface drawn on, when there is one: a phone takes it away in
    /// the background and hands a new one back, and the device, the
    /// pipeline and every atlas outlive that.
    surface: Option<wgpu::Surface<'static>>,
    config: wgpu::SurfaceConfiguration,
    view_format: wgpu::TextureFormat,
    pub pipeline: Pipeline,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    vertex_capacity: usize,
    index_buffer: wgpu::Buffer,
    index_quads: usize,
    /// Bind groups per atlas texture, by identity: a page with panes at
    /// their own font sizes draws from several atlases each frame.
    atlas_bind_groups: Vec<(usize, Rc<AtlasBindGroups>)>,
    scratch: Vec<Vertex>,
    pub adapter_info: wgpu::AdapterInfo,
}

impl Gpu {
    #[cfg(target_arch = "wasm32")]
    pub async fn new(canvas: web_sys::HtmlCanvasElement, width: u32, height: u32) -> Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            ..Default::default()
        });
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| anyhow!("creating the canvas surface: {e}"))?;
        Self::from_surface(&instance, surface, width, height).await
    }

    /// A device for `surface` and the pipeline for its format.
    pub async fn from_surface(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| anyhow!("no WebGPU adapter: {e}"))?;
        let adapter_info = adapter.get_info();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                required_features: wgpu::Features::empty(),
                // Never the WebGL2 downlevel limits: they cap textures at
                // 2048 where the desktop atlas may reach 8192.
                required_limits: wgpu::Limits::downlevel_defaults()
                    .using_resolution(adapter.limits()),
                label: None,
                memory_hints: Default::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| anyhow!("requesting the device: {e}"))?;
        let queue = Arc::new(queue);
        // A device error is fatal by default -- a panic on the core's
        // thread, and no word of why. Logged instead; the frame that hit
        // it is lost, the next one is tried.
        device.on_uncaptured_error(Box::new(|err| log::error!("wgpu: {err}")));

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .first()
            .copied()
            .ok_or_else(|| anyhow!("the canvas offers no texture format"))?;
        let view_format = srgb_format(format);
        let config = wgpu::SurfaceConfiguration {
            usage: Self::surface_usage(&caps),
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: Self::alpha_mode(&caps),
            view_formats: if view_format != format { vec![view_format] } else { vec![] },
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let pipeline = Pipeline::new(&device, view_format);
        let uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ShaderUniform"),
            size: std::mem::size_of::<ShaderUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uniform_bind_group = pipeline.uniform_bind_group(&device, &uniform_buffer);
        let vertex_capacity = 4096;
        let vertex_buffer = Self::make_vertex_buffer(&device, vertex_capacity);
        let index_quads = vertex_capacity / 4;
        let index_buffer = Self::make_index_buffer(&device, &queue, index_quads);
        Ok(Self {
            device,
            queue,
            surface: Some(surface),
            config,
            view_format,
            pipeline,
            uniform_buffer,
            uniform_bind_group,
            vertex_buffer,
            vertex_capacity,
            index_buffer,
            index_quads,
            atlas_bind_groups: Vec::new(),
            scratch: Vec::new(),
            adapter_info,
        })
    }

    fn make_vertex_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vertices"),
            size: (capacity * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    fn make_index_buffer(device: &wgpu::Device, queue: &wgpu::Queue, quads: usize) -> wgpu::Buffer {
        let indices = quad_indices(quads);
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("indices"),
            size: (indices.len() * 4) as u64,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buffer, 0, bytemuck::cast_slice(&indices));
        buffer
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Grow both buffers as needed and upload; returns the quad count.
    fn upload_vertices(&mut self, vertices: &[Vertex]) -> usize {
        let quads = vertices.len() / 4;
        if vertices.len() > self.vertex_capacity {
            self.vertex_capacity = vertices.len().next_power_of_two();
            self.vertex_buffer = Self::make_vertex_buffer(&self.device, self.vertex_capacity);
        }
        if quads > self.index_quads {
            self.index_quads = quads.next_power_of_two();
            self.index_buffer = Self::make_index_buffer(&self.device, &self.queue, self.index_quads);
        }
        if !vertices.is_empty() {
            self.queue
                .write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(vertices));
        }
        quads
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
    }

    pub fn has_surface(&self) -> bool {
        self.surface.is_some()
    }

    /// Let go of the surface: nothing is drawn until another is attached.
    /// The device, the pipeline and every atlas stay.
    pub fn detach_surface(&mut self) {
        self.surface = None;
    }

    /// Reading the frame back (the smoke tests' snapshots) needs the
    /// swapchain to allow it; an Android swapchain often does not, and
    /// asking anyway is a device error.
    fn surface_usage(caps: &wgpu::SurfaceCapabilities) -> wgpu::TextureUsages {
        let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        if caps.usages.contains(wgpu::TextureUsages::COPY_SRC) {
            usage |= wgpu::TextureUsages::COPY_SRC;
        }
        usage
    }

    /// Opaque where offered; else whatever the surface has (Android
    /// offers only Inherit on some drivers).
    fn alpha_mode(caps: &wgpu::SurfaceCapabilities) -> wgpu::CompositeAlphaMode {
        if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto)
        }
    }

    /// Draw on `surface` from now on. Its format may differ from the last
    /// one's, in which case the pipeline is rebuilt for it; the atlases
    /// are unaffected, and their bind groups are made again on first use.
    pub fn attach_surface(
        &mut self,
        adapter: &wgpu::Adapter,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let caps = surface.get_capabilities(adapter);
        let format = caps
            .formats
            .first()
            .copied()
            .ok_or_else(|| anyhow!("the surface offers no texture format"))?;
        let view_format = srgb_format(format);
        self.config.format = format;
        self.config.usage = Self::surface_usage(&caps);
        self.config.alpha_mode = Self::alpha_mode(&caps);
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.config.view_formats = if view_format != format { vec![view_format] } else { vec![] };
        surface.configure(&self.device, &self.config);
        if view_format != self.view_format {
            self.view_format = view_format;
            self.pipeline = Pipeline::new(&self.device, view_format);
            self.uniform_bind_group = self.pipeline.uniform_bind_group(&self.device, &self.uniform_buffer);
            self.atlas_bind_groups.clear();
        }
        self.surface = Some(surface);
        Ok(())
    }

    pub fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    pub(crate) fn forget_texture(&mut self, identity: usize) {
        self.atlas_bind_groups.retain(|(id, _)| *id != identity);
    }

    fn atlas_groups(&mut self, atlas: &GpuTexture) -> Rc<AtlasBindGroups> {
        let identity = atlas.id();
        if let Some(i) = self.atlas_bind_groups.iter().position(|(id, _)| *id == identity) {
            return Rc::clone(&self.atlas_bind_groups[i].1);
        }
        // Image textures join the font atlases. Draws own their groups so an
        // eviction cannot invalidate an earlier batch in the same frame.
        if self.atlas_bind_groups.len() >= 128 {
            self.atlas_bind_groups.remove(0);
        }
        let groups = Rc::new(self.pipeline.atlas_bind_groups(&self.device, &atlas.view()));
        self.atlas_bind_groups.push((identity, Rc::clone(&groups)));
        groups
    }

    fn prepare_batches(&mut self, batches: &[(&[Vertex], &GpuTexture)]) -> Vec<(u32, u32, Rc<AtlasBindGroups>)> {
        // Bind groups retain their textures, including deleted images.
        self.atlas_bind_groups.retain(|(id, _)| batches.iter().any(|(_, texture)| texture.id() == *id));
        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let mut ranges = Vec::with_capacity(batches.len());
        for (vertices, atlas) in batches {
            if vertices.is_empty() { continue; }
            let start = scratch.len() / 4;
            scratch.extend_from_slice(vertices);
            let end = scratch.len() / 4;
            ranges.push((start as u32, end as u32, self.atlas_groups(atlas)));
        }
        self.upload_vertices(&scratch);
        self.scratch = scratch;
        ranges
    }

    /// One frame from several batches, each from its own atlas: one pass,
    /// one vertex upload, one draw per batch.
    pub fn draw_batches(
        &mut self,
        batches: &[(&[Vertex], &GpuTexture)],
        background: [f32; 4],
        millis: u32,
    ) -> Result<()> {
        let (w, h) = self.size();
        let uniforms = ShaderUniform {
            foreground_text_hsb: [1.0, 1.0, 1.0],
            milliseconds: millis,
            viewport_and_corner: [w as f32, h as f32, 0.0, 0.0],
            window_border: [0.0; 4],
            projection: centred_projection(w as f32, h as f32),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));

        let ranges = self.prepare_batches(batches);

        let Some(surface) = &self.surface else {
            // Nothing to draw on: the quads were built for nothing, which
            // is cheap, and the next attached surface asks for a frame.
            return Ok(());
        };
        let frame = match surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost) | Err(wgpu::SurfaceError::Outdated) => {
                surface.configure(&self.device, &self.config);
                surface
                    .get_current_texture()
                    .map_err(|e| anyhow!("acquiring the frame after reconfiguring: {e}"))?
            }
            Err(err) => return Err(anyhow!("acquiring the frame: {err}")),
        };
        let view = view_as(&frame.texture, self.view_format);
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pane"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: background[0] as f64,
                            g: background[1] as f64,
                            b: background[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if !ranges.is_empty() {
                pass.set_pipeline(&self.pipeline.render_pipeline);
                pass.set_bind_group(0, &self.uniform_bind_group, &[]);
                pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
                pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                for (start, end, groups) in &ranges {
                    pass.set_bind_group(1, &groups.linear, &[]);
                    pass.set_bind_group(2, &groups.nearest, &[]);
                    pass.draw_indexed(start * 6..end * 6, 0, 0..1);
                }
            }
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }

    /// Render through the production pipeline in the canvas's format. Some
    /// swapchains prohibit copying, so probes read an offscreen target.
    pub async fn draw_and_read_pixel(
        &mut self,
        vertices: &[Vertex],
        atlas: &GpuTexture,
        x: u32,
        y: u32,
    ) -> Result<[u8; 4]> {
        self.draw_batches_and_read_pixel(&[(vertices, atlas)], x, y).await
    }

    pub async fn draw_batches_and_read_pixel(
        &mut self,
        batches: &[(&[Vertex], &GpuTexture)],
        x: u32,
        y: u32,
    ) -> Result<[u8; 4]> {
        let (w, h) = self.size();
        let uniforms = ShaderUniform {
            foreground_text_hsb: [1.0, 1.0, 1.0],
            milliseconds: 0,
            viewport_and_corner: [w as f32, h as f32, 0.0, 0.0],
            window_border: [0.0; 4],
            projection: centred_projection(w as f32, h as f32),
        };
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniforms));
        let ranges = self.prepare_batches(batches);
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("pixel probe"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2,
            format: self.view_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("check") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("check"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.pipeline.render_pipeline);
            pass.set_bind_group(0, &self.uniform_bind_group, &[]);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            for (start, end, groups) in &ranges {
                pass.set_bind_group(1, &groups.linear, &[]);
                pass.set_bind_group(2, &groups.nearest, &[]);
                pass.draw_indexed(start * 6..end * 6, 0, 0..1);
            }
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = futures::channel::oneshot::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = self.device.poll(wgpu::PollType::Poll);
        rx.await
            .map_err(|_| anyhow!("readback dropped"))?
            .map_err(|e| anyhow!("mapping the readback: {e}"))?;
        let bytes = slice.get_mapped_range();
        let mut pixel = [bytes[0], bytes[1], bytes[2], bytes[3]];
        // Byte order follows the canvas format.
        if matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        ) {
            pixel.swap(0, 2);
        }
        drop(bytes);
        readback.unmap();
        Ok(pixel)
    }
}
