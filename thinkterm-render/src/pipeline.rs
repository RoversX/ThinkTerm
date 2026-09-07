//! The wgpu side of the renderer: the one shader compiled into a pipeline,
//! the bind-group layouts and samplers it expects, the uniform block, the
//! centred projection, and an atlas texture the `Atlas` can upload into.
//! Backend-neutral in the sense that matters here: a desktop window surface
//! and a browser canvas both configure it with their own target format.

use crate::bitmaps::{BitmapImage, Texture2d};
use crate::geom::Rect;
use crate::vertex::Vertex;
use std::sync::Arc;

/// Matches `ShaderUniform` in shader.wgsl field for field.
#[repr(C)]
#[derive(Copy, Clone, Default, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ShaderUniform {
    pub foreground_text_hsb: [f32; 3],
    pub milliseconds: u32,
    pub viewport_and_corner: [f32; 4],
    pub window_border: [f32; 4],
    pub projection: [[f32; 4]; 4],
}

/// The projection every ThinkTerm frame draws with: orthographic, centred
/// on the surface, so quad positions are offsets from the middle. The
/// transposed array is what the WGSL expects for a symmetric ortho.
pub fn centred_projection(width: f32, height: f32) -> [[f32; 4]; 4] {
    euclid::Transform3D::<f32, f32, f32>::ortho(
        -width / 2.0,
        width / 2.0,
        height / 2.0,
        -height / 2.0,
        -1.0,
        1.0,
    )
    .to_arrays_transposed()
}

/// The sRGB name of a target format: WebGPU canvases can only be configured
/// with the linear name, and are then viewed as sRGB so the hardware encodes
/// on write, exactly as the desktop's already-sRGB surface does.
pub fn srgb_format(format: wgpu::TextureFormat) -> wgpu::TextureFormat {
    format.add_srgb_suffix()
}

/// A view of `texture` as `format`; the view a frame is rendered into.
pub fn view_as(texture: &wgpu::Texture, format: wgpu::TextureFormat) -> wgpu::TextureView {
    texture.create_view(&wgpu::TextureViewDescriptor {
        format: Some(format),
        ..Default::default()
    })
}

/// The compiled shader and everything a draw binds to it.
pub struct Pipeline {
    pub render_pipeline: wgpu::RenderPipeline,
    pub uniform_layout: wgpu::BindGroupLayout,
    pub texture_layout: wgpu::BindGroupLayout,
    pub nearest_sampler: wgpu::Sampler,
    pub linear_sampler: wgpu::Sampler,
    /// The colour target format the pipeline was built for.
    pub target_format: wgpu::TextureFormat,
}

pub struct AtlasBindGroups {
    pub linear: wgpu::BindGroup,
    pub nearest: wgpu::BindGroup,
}

impl Pipeline {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shader.wgsl"),
            source: wgpu::ShaderSource::Wgsl(crate::SHADER_SOURCE.into()),
        });

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
            label: Some("ShaderUniform bind group layout"),
        });

        let sampler = |filter: wgpu::FilterMode| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: filter,
                min_filter: filter,
                mipmap_filter: filter,
                ..Default::default()
            })
        };
        let nearest_sampler = sampler(wgpu::FilterMode::Nearest);
        let linear_sampler = sampler(wgpu::FilterMode::Linear);

        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
            label: Some("texture bind group layout"),
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("Render Pipeline Layout"),
            bind_group_layouts: &[&uniform_layout, &texture_layout, &texture_layout],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[Vertex::desc()],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: 1,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            multiview: None,
            cache: None,
        });

        Self {
            render_pipeline,
            uniform_layout,
            texture_layout,
            nearest_sampler,
            linear_sampler,
            target_format,
        }
    }

    pub fn uniform_bind_group(&self, device: &wgpu::Device, buffer: &wgpu::Buffer) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &self.uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
            label: Some("ShaderUniform bind group"),
        })
    }

    /// Both sampler flavours of the atlas: the shader picks linear when a
    /// quad is minified and nearest otherwise.
    pub fn atlas_bind_groups(&self, device: &wgpu::Device, view: &wgpu::TextureView) -> AtlasBindGroups {
        let make = |sampler: &wgpu::Sampler, label: &str| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                layout: &self.texture_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                ],
                label: Some(label),
            })
        };
        AtlasBindGroups {
            linear: make(&self.linear_sampler, "atlas linear"),
            nearest: make(&self.nearest_sampler, "atlas nearest"),
        }
    }
}

/// The index pattern for quads laid out as `Vertex` corners TL, TR, BL, BR:
/// two triangles per quad, six indices.
pub fn quad_indices(num_quads: usize) -> Vec<u32> {
    use crate::vertex::{VERTICES_PER_CELL, V_BOT_LEFT, V_BOT_RIGHT, V_TOP_LEFT, V_TOP_RIGHT};
    let mut indices = Vec::with_capacity(num_quads * 6);
    for q in 0..num_quads {
        let idx = (q * VERTICES_PER_CELL) as u32;
        indices.push(idx + V_TOP_LEFT as u32);
        indices.push(idx + V_TOP_RIGHT as u32);
        indices.push(idx + V_BOT_LEFT as u32);
        indices.push(idx + V_TOP_RIGHT as u32);
        indices.push(idx + V_BOT_LEFT as u32);
        indices.push(idx + V_BOT_RIGHT as u32);
    }
    indices
}

/// An RGBA sRGB texture on the device that the `Atlas` writes sprites into.
pub struct GpuTexture {
    texture: wgpu::Texture,
    queue: Arc<wgpu::Queue>,
    width: u32,
    height: u32,
    /// Distinct for every texture ever made, so a cache keyed by it cannot
    /// mistake a new texture at an old address for the old one.
    id: usize,
}

static NEXT_TEXTURE_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);

impl GpuTexture {
    pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    pub fn new(
        device: &wgpu::Device,
        queue: Arc<wgpu::Queue>,
        width: u32,
        height: u32,
    ) -> anyhow::Result<Self> {
        let limit = device.limits().max_texture_dimension_2d;
        if width > limit || height > limit {
            // wgpu panics rather than failing on an oversized texture.
            anyhow::bail!(
                "texture dimensions {width}x{height} exceed the max dimension {limit} \
                 supported by this GPU"
            );
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            label: Some("Texture Atlas"),
            view_formats: &[],
        });
        Ok(Self {
            texture,
            queue,
            width,
            height,
            id: NEXT_TEXTURE_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        })
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub fn view(&self) -> wgpu::TextureView {
        self.texture.create_view(&Default::default())
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }
}

impl Texture2d for GpuTexture {
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
        unimplemented!("reading back the atlas is not supported");
    }

    fn width(&self) -> usize {
        self.width as usize
    }

    fn height(&self) -> usize {
        self.height as usize
    }
}
