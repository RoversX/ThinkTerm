//! The canvas surface and one frame's worth of drawing through the shared
//! pipeline. Colour: the canvas is configured with the linear name of its
//! format and drawn through an sRGB view, so the hardware encodes on write
//! exactly as it does for the desktop's already-sRGB surface.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use thinkterm_render::pipeline::{
    centred_projection, quad_indices, srgb_format, view_as, AtlasBindGroups, GpuTexture, Pipeline,
    ShaderUniform,
};
use thinkterm_render::vertex::Vertex;

pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: Arc<wgpu::Queue>,
    surface: wgpu::Surface<'static>,
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
    atlas_bind_groups: Vec<(usize, AtlasBindGroups)>,
    scratch: Vec<Vertex>,
    pub adapter_info: wgpu::AdapterInfo,
}

impl Gpu {
    pub async fn new(canvas: web_sys::HtmlCanvasElement, width: u32, height: u32) -> Result<Self> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::BROWSER_WEBGPU,
            ..Default::default()
        });
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas))
            .map_err(|e| anyhow!("creating the canvas surface: {e}"))?;
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

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .first()
            .copied()
            .ok_or_else(|| anyhow!("the canvas offers no texture format"))?;
        let view_format = srgb_format(format);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
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
            surface,
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
        self.surface.configure(&self.device, &self.config);
    }

    pub fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    fn atlas_groups(&mut self, atlas: &GpuTexture) -> usize {
        let identity = atlas.id();
        if let Some(i) = self.atlas_bind_groups.iter().position(|(id, _)| *id == identity) {
            return i;
        }
        // A handful at a time is the most a page has; old ones go.
        if self.atlas_bind_groups.len() >= 8 {
            self.atlas_bind_groups.remove(0);
        }
        let groups = self.pipeline.atlas_bind_groups(&self.device, &atlas.view());
        self.atlas_bind_groups.push((identity, groups));
        self.atlas_bind_groups.len() - 1
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

        let mut scratch = std::mem::take(&mut self.scratch);
        scratch.clear();
        let mut ranges = Vec::with_capacity(batches.len());
        for (vertices, atlas) in batches {
            let start = scratch.len() / 4;
            scratch.extend_from_slice(vertices);
            let end = scratch.len() / 4;
            let group = self.atlas_groups(atlas);
            if end > start {
                ranges.push((start as u32, end as u32, group));
            }
        }
        self.upload_vertices(&scratch);
        self.scratch = scratch;

        let frame = self
            .surface
            .get_current_texture()
            .map_err(|e| anyhow!("acquiring the canvas frame: {e}"))?;
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
                for (start, end, group) in ranges {
                    let groups = &self.atlas_bind_groups[group].1;
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

    /// Render one frame of `vertices` and read back the pixel at (x, y) as
    /// the 8-bit value the canvas holds. The check the smoke test runs:
    /// a known linear colour must come back sRGB-encoded.
    pub async fn draw_and_read_pixel(
        &mut self,
        vertices: &[Vertex],
        atlas: &GpuTexture,
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
        let quads = self.upload_vertices(vertices);
        let group = self.atlas_groups(atlas);

        let frame = self
            .surface
            .get_current_texture()
            .map_err(|e| anyhow!("acquiring the canvas frame: {e}"))?;
        let view = view_as(&frame.texture, self.view_format);
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
            let groups = &self.atlas_bind_groups[group].1;
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
            pass.set_bind_group(1, &groups.linear, &[]);
            pass.set_bind_group(2, &groups.nearest, &[]);
            pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            pass.set_index_buffer(self.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..(quads * 6) as u32, 0, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &frame.texture,
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
        frame.present();

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
