use crate::colorease::ColorEaseUniform;
use crate::renderstate::{LoggedSrgbTexture2d, RenderState};
use crate::termwindow::webgpu::{ShaderUniform, WebGpuState};
use crate::termwindow::RenderFrame;
use crate::uniforms::UniformBuilder;
use ::window::color::LinearRgba;
use ::window::glium;
use ::window::glium::uniforms::{
    MagnifySamplerFilter, MinifySamplerFilter, Sampler, SamplerWrapFunction,
};
use ::window::glium::{BlendingFunction, LinearBlendingFactor, Surface};
use ::window::{Appearance, Dimensions, WindowDecorations, WindowState};
use config::FreeTypeLoadTarget;

const LINUX_WINDOW_CORNER_RADIUS: f32 = 16.0;
const LINUX_WINDOW_BORDER_WIDTH: f32 = 1.0;

/// Grow-only scratch GPU buffers shared by every card render and composite
/// draw of a frame: one vertex upload, sliced per draw with `base_vertex`.
pub(crate) struct CardScratch {
    vb: wgpu::Buffer,
    vb_capacity_verts: usize,
    index: wgpu::Buffer,
    index_capacity_quads: usize,
}

impl CardScratch {
    fn ensure(
        scratch: &mut Option<CardScratch>,
        state: &WebGpuState,
        verts_needed: usize,
        quads_needed: usize,
    ) {
        use crate::quad::{VERTICES_PER_CELL, V_BOT_LEFT, V_BOT_RIGHT, V_TOP_LEFT, V_TOP_RIGHT};
        const INDICES_PER_CELL: usize = 6;
        let need_vb = scratch
            .as_ref()
            .map_or(true, |s| s.vb_capacity_verts < verts_needed);
        let need_index = scratch
            .as_ref()
            .map_or(true, |s| s.index_capacity_quads < quads_needed);
        if !need_vb && !need_index {
            return;
        }
        let vb_capacity_verts = scratch
            .as_ref()
            .map(|s| s.vb_capacity_verts)
            .unwrap_or(0)
            .max(verts_needed)
            .next_power_of_two()
            .max(1024);
        let index_capacity_quads = scratch
            .as_ref()
            .map(|s| s.index_capacity_quads)
            .unwrap_or(0)
            .max(quads_needed)
            .next_power_of_two()
            .max(256);
        let vb = state.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("Card Scratch Vertices"),
            size: (vb_capacity_verts * std::mem::size_of::<crate::quad::Vertex>())
                as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut indices: Vec<u32> = Vec::with_capacity(index_capacity_quads * INDICES_PER_CELL);
        for q in 0..index_capacity_quads {
            let idx = (q * VERTICES_PER_CELL) as u32;
            indices.push(idx + V_TOP_LEFT as u32);
            indices.push(idx + V_TOP_RIGHT as u32);
            indices.push(idx + V_BOT_LEFT as u32);
            indices.push(idx + V_TOP_RIGHT as u32);
            indices.push(idx + V_BOT_LEFT as u32);
            indices.push(idx + V_BOT_RIGHT as u32);
        }
        let index = wgpu::util::DeviceExt::create_buffer_init(
            &state.device,
            &wgpu::util::BufferInitDescriptor {
                label: Some("Card Scratch Indices"),
                usage: wgpu::BufferUsages::INDEX,
                contents: bytemuck::cast_slice(&indices),
            },
        );
        *scratch = Some(CardScratch {
            vb,
            vb_capacity_verts,
            index,
            index_capacity_quads,
        });
    }
}

/// The card texture work of one frame, handed from the paint pass to the
/// draw: heaps to render into card textures, and the textured quads that
/// stand in for the cards in the main pass.
pub(crate) struct CardDrawData {
    pub pending: Vec<crate::termwindow::render::paint::PendingCardRender>,
    pub composites: Vec<crate::termwindow::render::paint::CardComposite>,
}

/// Build the 4 vertices of one composite quad: `dest` cropped to `clip`,
/// UVs following the crop, positions converted to the window-centre-origin
/// space the shared projection expects.
fn composite_quad_verts(
    composite: &crate::termwindow::render::paint::CardComposite,
    dimensions: &Dimensions,
) -> Option<[crate::quad::Vertex; 4]> {
    use crate::quad::Vertex;
    let dest = composite.dest;
    let vis = dest.intersection(&composite.clip)?;
    if vis.size.width <= 0.0 || vis.size.height <= 0.0 {
        return None;
    }
    let half_w = dimensions.pixel_width as f32 / 2.0;
    let half_h = dimensions.pixel_height as f32 / 2.0;
    let u0 = (vis.min_x() - dest.min_x()) / dest.size.width;
    let u1 = (vis.max_x() - dest.min_x()) / dest.size.width;
    let v0 = (vis.min_y() - dest.min_y()) / dest.size.height;
    let v1 = (vis.max_y() - dest.min_y()) / dest.size.height;
    let (x0, x1) = (vis.min_x() - half_w, vis.max_x() - half_w);
    let (y0, y1) = (vis.min_y() - half_h, vis.max_y() - half_h);
    const IS_BG_IMAGE: f32 = 2.0;
    let vert = |x: f32, y: f32, u: f32, v: f32| Vertex {
        position: [x, y],
        tex: [u, v],
        fg_color: [1.0, 1.0, 1.0, composite.opacity],
        alt_color: [1.0, 1.0, 1.0, composite.opacity],
        hsv: [1.0, 1.0, 1.0],
        has_color: IS_BG_IMAGE,
        mix_value: 0.0,
    };
    // Corner order matches the shared index pattern: TL, TR, BL, BR.
    Some([
        vert(x0, y0, u0, v0),
        vert(x1, y0, u1, v0),
        vert(x0, y1, u0, v1),
        vert(x1, y1, u1, v1),
    ])
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WindowBorder {
    pub width: f32,
    pub color: [f32; 3],
}

impl WindowBorder {
    const NONE: Self = Self {
        width: 0.0,
        color: [0.0; 3],
    };
}

pub(crate) fn effective_window_corner_radius(
    decorations: WindowDecorations,
    window_state: WindowState,
    dpi: usize,
) -> f32 {
    if cfg!(target_os = "linux")
        && crate::termwindow::ui::platform_chrome::uses_integrated_window_buttons(
            decorations,
            window_state,
        )
        && window_state.can_resize()
        && !window_state.contains(WindowState::TILED)
        && window_state.contains(WindowState::COMPOSITED)
    {
        LINUX_WINDOW_CORNER_RADIUS * dpi.max(1) as f32 / 96.0
    } else {
        0.0
    }
}

pub(crate) fn effective_window_border(
    decorations: WindowDecorations,
    window_state: WindowState,
    dpi: usize,
    appearance: Appearance,
) -> WindowBorder {
    if !cfg!(target_os = "linux")
        || !crate::termwindow::ui::platform_chrome::uses_integrated_window_buttons(
            decorations,
            window_state,
        )
        || !window_state.can_resize()
        || window_state.contains(WindowState::TILED)
    {
        return WindowBorder::NONE;
    }

    let color = match appearance {
        Appearance::Light | Appearance::LightHighContrast => {
            LinearRgba::with_srgba(199, 199, 204, 255)
        }
        Appearance::Dark | Appearance::DarkHighContrast => LinearRgba::with_srgba(68, 68, 76, 255),
    };
    WindowBorder {
        width: LINUX_WINDOW_BORDER_WIDTH * dpi.max(1) as f32 / 96.0,
        color: [color.0, color.1, color.2],
    }
}

pub(crate) fn draw_webgpu_layers(
    webgpu: &WebGpuState,
    render_state: &RenderState,
    dimensions: Dimensions,
    foreground_text_hsb: [f32; 3],
    milliseconds: u32,
    clear_color: wgpu::Color,
    corner_radius: f32,
    window_border: WindowBorder,
    window_label: usize,
    cards: CardDrawData,
    card_scratch: &mut Option<CardScratch>,
    frame_verts: &mut Vec<crate::quad::Vertex>,
) -> anyhow::Result<()> {
    let acquire_start = crate::perf::now();
    let output = webgpu.surface.get_current_texture()?;
    crate::perf::log_duration("webgpu_surface_acquire", acquire_start);
    let view = output
        .texture
        .create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = webgpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        });
    let tex = render_state.glyph_cache.borrow().atlas.texture();
    let (texture_linear_bind_group, texture_nearest_bind_group) = webgpu.atlas_bind_groups(&tex);

    let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
        -(dimensions.pixel_width as f32) / 2.0,
        dimensions.pixel_width as f32 / 2.0,
        dimensions.pixel_height as f32 / 2.0,
        -(dimensions.pixel_height as f32) / 2.0,
        -1.0,
        1.0,
    )
    .to_arrays_transposed();

    // Every non-card pass in this frame uses the same uniform value, so one
    // persistent slot serves the layer passes and the composite passes alike.
    let window_uniforms = webgpu.window_uniform(ShaderUniform {
        foreground_text_hsb,
        milliseconds,
        viewport_and_corner: [
            dimensions.pixel_width as f32,
            dimensions.pixel_height as f32,
            corner_radius,
            0.0,
        ],
        window_border: [
            window_border.color[0],
            window_border.color[1],
            window_border.color[2],
            window_border.width,
        ],
        projection,
    });

    // ---- Card texture work -------------------------------------------------
    // One combined vertex upload covers every card render pass and every
    // composite quad; each draw slices it with `base_vertex`. The uploads
    // must all happen before any pass is encoded because queued buffer
    // writes run at the head of the submit. The pending cards' vertices were
    // already extracted into `frame_verts` by the paint pass; only the
    // composite quads are appended here.
    let card_pass_start = crate::perf::now();
    let mut composite_draws: Vec<(i8, i32, &crate::termwindow::render::paint::CardComposite)> =
        Vec::with_capacity(cards.composites.len());
    for composite in &cards.composites {
        if let Some(verts) = composite_quad_verts(composite, &dimensions) {
            let base = frame_verts.len();
            frame_verts.extend_from_slice(&verts);
            composite_draws.push((composite.zindex, base as i32, composite));
        }
    }
    if !frame_verts.is_empty() {
        let max_quads = cards
            .pending
            .iter()
            .map(|pending| pending.quad_count)
            .max()
            .unwrap_or(0)
            .max(1);
        CardScratch::ensure(card_scratch, webgpu, frame_verts.len(), max_quads);
        let scratch = card_scratch.as_ref().expect("just ensured");
        webgpu
            .queue
            .write_buffer(&scratch.vb, 0, bytemuck::cast_slice(frame_verts));

        // Render each dirty card's quads into its texture. These passes are
        // encoded before the main pass, so the composites below sample the
        // fresh picture.
        for (card_slot, pending) in cards.pending.iter().enumerate() {
            let (base, quads) = (pending.first_vertex, pending.quad_count);
            if quads == 0 {
                continue;
            }
            let half_w = dimensions.pixel_width as f32 / 2.0;
            let half_h = dimensions.pixel_height as f32 / 2.0;
            let area = pending.area;
            // `to_arrays()`, NOT `to_arrays_transposed()`: euclid's ortho
            // keeps its translation in the fourth row, and the shader
            // multiplies matrix * column-vector, so the plain row arrays are
            // already the WGSL column layout. The main pass gets away with
            // the transposed form only because its ortho is symmetric --
            // translation is zero there, and a transposed diagonal is
            // itself. This ortho is off-centre; transposing it puts the
            // translation into the w row and every card collapses into a
            // perspective wedge.
            let card_projection = euclid::Transform3D::<f32, f32, f32>::ortho(
                area.min_x() - half_w,
                area.max_x() - half_w,
                area.max_y() - half_h,
                area.min_y() - half_h,
                -1.0,
                1.0,
            )
            .to_arrays();
            let card_uniforms = webgpu.card_uniform(card_slot, ShaderUniform {
                foreground_text_hsb,
                milliseconds,
                viewport_and_corner: [
                    pending.texture.width as f32,
                    pending.texture.height as f32,
                    0.0,
                    0.0,
                ],
                window_border: [0.0; 4],
                projection: card_projection,
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Card Render Pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &pending.texture.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&webgpu.render_pipeline);
            pass.set_bind_group(0, &card_uniforms, &[]);
            pass.set_bind_group(1, &texture_linear_bind_group, &[]);
            pass.set_bind_group(2, &texture_nearest_bind_group, &[]);
            pass.set_vertex_buffer(0, scratch.vb.slice(..));
            pass.set_index_buffer(scratch.index.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..(quads * 6) as u32, base as i32, 0..1);
        }
    }
    crate::perf::log_duration("card_texture_passes", card_pass_start);
    crate::perf::log_counter("card_texture_renders", cards.pending.len());
    crate::perf::log_counter("card_composites", composite_draws.len());
    // -----------------------------------------------------------------------

    let mut cleared = false;
    let mut draw_calls = 0usize;
    let mut vertices_total = 0usize;
    let draw_start = crate::perf::now();
    for layer in render_state.layers.borrow().iter() {
        for idx in 0..3 {
            let vb = &layer.vb.borrow()[idx];
            let (vertex_count, index_count) = vb.vertex_index_count();
            if vertex_count > 0 {
                let vertices = vb.current_vb_mut();
                let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("Render Pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if cleared {
                                wgpu::LoadOp::Load
                            } else {
                                wgpu::LoadOp::Clear(clear_color)
                            },
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
                cleared = true;

                render_pass.set_pipeline(&webgpu.render_pipeline);
                render_pass.set_bind_group(0, &window_uniforms, &[]);
                render_pass.set_bind_group(1, &texture_linear_bind_group, &[]);
                render_pass.set_bind_group(2, &texture_nearest_bind_group, &[]);
                // Upload only the quads this frame actually wrote into the
                // persistent vertex buffer. The old scheme allocated a fresh
                // `mapped_at_creation` buffer every frame, which wgpu
                // zero-fills -- megabytes of allocation and memset per layer
                // per frame, measured at ~10% of the main thread.
                let upload_start = crate::perf::now();
                vertices.webgpu().upload(vertex_count);
                crate::perf::log_duration("webgpu_vb_upload", upload_start);
                crate::perf::log_counter("webgpu_vb_bytes", vertices.webgpu().capacity_bytes());
                render_pass.set_vertex_buffer(0, vertices.webgpu().slice(..));
                render_pass
                    .set_index_buffer(vb.indices.webgpu().slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..index_count as _, 0, 0..1);
                draw_calls += 1;
                vertices_total += vertex_count;
            }

            // Card pictures composite between this layer's base fills and
            // its glyph sub-buffers: above their own card background, below
            // every label drawn on top.
            if idx == 0
                && composite_draws
                    .iter()
                    .any(|(zindex, _, _)| *zindex == layer.zindex())
            {
                let scratch = card_scratch
                    .as_ref()
                    .expect("composite draws imply the scratch exists");
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("Card Composite Pass"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: if cleared {
                                wgpu::LoadOp::Load
                            } else {
                                wgpu::LoadOp::Clear(clear_color)
                            },
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    occlusion_query_set: None,
                    timestamp_writes: None,
                });
                cleared = true;
                pass.set_pipeline(&webgpu.render_pipeline);
                pass.set_bind_group(0, &window_uniforms, &[]);
                pass.set_vertex_buffer(0, scratch.vb.slice(..));
                pass.set_index_buffer(scratch.index.slice(..), wgpu::IndexFormat::Uint32);
                for (zindex, base, composite) in &composite_draws {
                    if *zindex != layer.zindex() {
                        continue;
                    }
                    pass.set_bind_group(1, &composite.texture.bind_group, &[]);
                    pass.set_bind_group(2, &composite.texture.bind_group, &[]);
                    pass.draw_indexed(0..6, *base, 0..1);
                    draw_calls += 1;
                }
            }

            vb.next_index();
        }
    }

    crate::perf::log_duration("webgpu_encode", draw_start);
    crate::perf::log_counter("webgpu_draw_calls", draw_calls);
    crate::perf::log_counter("webgpu_vertices", vertices_total);
    let submit_start = crate::perf::now();
    webgpu.queue.submit(std::iter::once(encoder.finish()));
    if crate::framedump::should_dump(window_label) {
        if let Err(err) = crate::framedump::dump_texture(
            &webgpu.device,
            &webgpu.queue,
            &output.texture,
            window_label,
        ) {
            log::error!("framedump failed: {err:#}");
        }
        // Ground truth for the card texture path: dump each card texture
        // rendered this frame under a distinctive label.
        for (i, pending) in cards.pending.iter().enumerate() {
            if let Err(err) = crate::framedump::dump_texture(
                &webgpu.device,
                &webgpu.queue,
                &pending.texture.texture,
                90000 + i,
            ) {
                log::error!("card framedump failed: {err:#}");
            }
        }
    }
    output.present();
    crate::perf::log_duration("webgpu_submit_present", submit_start);

    Ok(())
}

impl crate::TermWindow {
    pub fn call_draw(&mut self, frame: &mut RenderFrame) -> anyhow::Result<()> {
        match frame {
            RenderFrame::Glium(ref mut frame) => self.call_draw_glium(frame),
            RenderFrame::WebGpu => self.call_draw_webgpu(),
        }
    }

    fn call_draw_webgpu(&mut self) -> anyhow::Result<()> {
        let webgpu = self.webgpu.as_mut().unwrap();
        let render_state = self.render_state.as_ref().unwrap();
        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = [
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        ];

        let milliseconds = self.created.elapsed().as_millis() as u32;
        let corner_radius = effective_window_corner_radius(
            self.config.window_decorations,
            self.window_state,
            self.dimensions.dpi,
        );
        let window_border = effective_window_border(
            self.config.window_decorations,
            self.window_state,
            self.dimensions.dpi,
            crate::native_settings::effective_appearance(),
        );
        let cards = CardDrawData {
            pending: std::mem::take(&mut *self.pending_card_renders.borrow_mut()),
            composites: std::mem::take(&mut *self.card_composites.borrow_mut()),
        };
        let mut card_scratch = self.card_scratch.borrow_mut();
        let mut frame_verts = self.card_frame_verts.borrow_mut();
        draw_webgpu_layers(
            webgpu,
            render_state,
            self.dimensions,
            foreground_text_hsb,
            milliseconds,
            wgpu::Color {
                r: 0.,
                g: 0.,
                b: 0.,
                a: 0.,
            },
            corner_radius,
            window_border,
            self.mux_window_id as usize,
            cards,
            &mut card_scratch,
            &mut frame_verts,
        )?;
        render_state.maybe_shrink_quads();
        Ok(())
    }

    fn call_draw_glium(&mut self, frame: &mut glium::Frame) -> anyhow::Result<()> {
        use window::glium::texture::SrgbTexture2d;

        let gl_state = self.render_state.as_ref().unwrap();
        let tex = gl_state.glyph_cache.borrow().atlas.texture();
        let tex = if let Some(tex) = tex.downcast_ref::<SrgbTexture2d>() {
            tex
        } else {
            tex.downcast_ref::<LoggedSrgbTexture2d>()
                .expect("OpenGL texture atlas")
                .inner()
        };

        frame.clear_color(0., 0., 0., 0.);

        let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
            -(self.dimensions.pixel_width as f32) / 2.0,
            self.dimensions.pixel_width as f32 / 2.0,
            self.dimensions.pixel_height as f32 / 2.0,
            -(self.dimensions.pixel_height as f32) / 2.0,
            -1.0,
            1.0,
        )
        .to_arrays_transposed();

        let use_subpixel = match self
            .config
            .freetype_render_target
            .unwrap_or(self.config.freetype_load_target)
        {
            FreeTypeLoadTarget::HorizontalLcd | FreeTypeLoadTarget::VerticalLcd => true,
            _ => false,
        };

        let dual_source_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceOneColor,
                    destination: LinearBlendingFactor::OneMinusSourceOneColor,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },

            ..Default::default()
        };

        let alpha_blending = glium::DrawParameters {
            blend: glium::Blend {
                color: BlendingFunction::Addition {
                    source: LinearBlendingFactor::SourceAlpha,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                alpha: BlendingFunction::Addition {
                    source: LinearBlendingFactor::One,
                    destination: LinearBlendingFactor::OneMinusSourceAlpha,
                },
                constant_value: (0.0, 0.0, 0.0, 0.0),
            },
            ..Default::default()
        };

        // Clamp and use the nearest texel rather than interpolate.
        // This prevents things like the box cursor outlines from
        // being randomly doubled in width or height
        let atlas_nearest_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Nearest)
            .minify_filter(MinifySamplerFilter::Nearest);

        let atlas_linear_sampler = Sampler::new(&*tex)
            .wrap_function(SamplerWrapFunction::Clamp)
            .magnify_filter(MagnifySamplerFilter::Linear)
            .minify_filter(MinifySamplerFilter::Linear);

        let foreground_text_hsb = self.config.foreground_text_hsb;
        let foreground_text_hsb = (
            foreground_text_hsb.hue,
            foreground_text_hsb.saturation,
            foreground_text_hsb.brightness,
        );

        let milliseconds = self.created.elapsed().as_millis() as u32;
        let corner_radius = effective_window_corner_radius(
            self.config.window_decorations,
            self.window_state,
            self.dimensions.dpi,
        );
        let window_clip = (
            self.dimensions.pixel_width as f32,
            self.dimensions.pixel_height as f32,
            corner_radius,
        );
        let window_border = effective_window_border(
            self.config.window_decorations,
            self.window_state,
            self.dimensions.dpi,
            crate::native_settings::effective_appearance(),
        );
        let window_border = (
            window_border.color[0],
            window_border.color[1],
            window_border.color[2],
            window_border.width,
        );

        let cursor_blink: ColorEaseUniform = (*self.cursor_blink_state.borrow()).into();
        let blink: ColorEaseUniform = (*self.blink_state.borrow()).into();
        let rapid_blink: ColorEaseUniform = (*self.rapid_blink_state.borrow()).into();

        for layer in gl_state.layers.borrow().iter() {
            for idx in 0..3 {
                let vb = &layer.vb.borrow()[idx];
                let (vertex_count, index_count) = vb.vertex_index_count();
                if vertex_count > 0 {
                    let vertices = vb.current_vb_mut();
                    let subpixel_aa = use_subpixel && idx == 1;

                    let mut uniforms = UniformBuilder::default();

                    uniforms.add("projection", &projection);
                    uniforms.add("atlas_nearest_sampler", &atlas_nearest_sampler);
                    uniforms.add("atlas_linear_sampler", &atlas_linear_sampler);
                    uniforms.add("foreground_text_hsb", &foreground_text_hsb);
                    uniforms.add("subpixel_aa", &subpixel_aa);
                    uniforms.add("milliseconds", &milliseconds);
                    uniforms.add("window_clip", &window_clip);
                    uniforms.add("window_border", &window_border);
                    uniforms.add_struct("cursor_blink", &cursor_blink);
                    uniforms.add_struct("blink", &blink);
                    uniforms.add_struct("rapid_blink", &rapid_blink);

                    frame.draw(
                        vertices.glium().slice(0..vertex_count).unwrap(),
                        vb.indices.glium().slice(0..index_count).unwrap(),
                        gl_state.glyph_prog.as_ref().unwrap(),
                        &uniforms,
                        if subpixel_aa {
                            &dual_source_blending
                        } else {
                            &alpha_blending
                        },
                    )?;
                }

                vb.next_index();
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_corner_radius_only_applies_to_floating_client_chrome() {
        let decorations = WindowDecorations::INTEGRATED_BUTTONS | WindowDecorations::RESIZE;
        let radius = effective_window_corner_radius(decorations, WindowState::COMPOSITED, 96);
        if cfg!(target_os = "linux") {
            assert_eq!(radius, LINUX_WINDOW_CORNER_RADIUS);
        } else {
            assert_eq!(radius, 0.0);
        }

        for state in [
            WindowState::MAXIMIZED,
            WindowState::FULL_SCREEN,
            WindowState::TILED,
            WindowState::SERVER_DECORATED,
        ] {
            assert_eq!(
                effective_window_corner_radius(decorations, state | WindowState::COMPOSITED, 96,),
                0.0
            );
        }
        assert_eq!(
            effective_window_corner_radius(
                WindowDecorations::TITLE | WindowDecorations::RESIZE,
                WindowState::COMPOSITED,
                96,
            ),
            0.0
        );
        assert_eq!(
            effective_window_corner_radius(decorations, WindowState::default(), 96),
            0.0
        );
    }

    #[test]
    fn linux_window_border_tracks_appearance_and_floating_state() {
        let decorations = WindowDecorations::INTEGRATED_BUTTONS | WindowDecorations::RESIZE;
        let dark =
            effective_window_border(decorations, WindowState::COMPOSITED, 96, Appearance::Dark);
        let light =
            effective_window_border(decorations, WindowState::COMPOSITED, 96, Appearance::Light);
        if cfg!(target_os = "linux") {
            assert_eq!(dark.width, 1.0);
            assert_eq!(light.width, 1.0);
            assert_ne!(dark.color, light.color);
            assert_eq!(
                effective_window_border(
                    decorations,
                    WindowState::COMPOSITED,
                    192,
                    Appearance::Dark,
                )
                .width,
                2.0
            );
        } else {
            assert_eq!(dark, WindowBorder::NONE);
            assert_eq!(light, WindowBorder::NONE);
        }

        for state in [
            WindowState::MAXIMIZED,
            WindowState::FULL_SCREEN,
            WindowState::TILED,
            WindowState::SERVER_DECORATED,
        ] {
            assert_eq!(
                effective_window_border(
                    decorations,
                    state | WindowState::COMPOSITED,
                    96,
                    Appearance::Dark,
                ),
                WindowBorder::NONE
            );
        }
    }
}
