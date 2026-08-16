use crate::colorease::ColorEaseUniform;
use crate::renderstate::{LoggedSrgbTexture2d, RenderState};
use crate::termwindow::webgpu::{ShaderUniform, WebGpuState, WebGpuTexture};
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
    let tex = tex.downcast_ref::<WebGpuTexture>().unwrap();
    let texture_view = tex.create_view(&wgpu::TextureViewDescriptor::default());

    let texture_linear_bind_group = webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        layout: &webgpu.texture_bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&webgpu.texture_linear_sampler),
            },
        ],
        label: Some("linear bind group"),
    });

    let texture_nearest_bind_group = webgpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
        layout: &webgpu.texture_bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&webgpu.texture_nearest_sampler),
            },
        ],
        label: Some("nearest bind group"),
    });

    let projection = euclid::Transform3D::<f32, f32, f32>::ortho(
        -(dimensions.pixel_width as f32) / 2.0,
        dimensions.pixel_width as f32 / 2.0,
        dimensions.pixel_height as f32 / 2.0,
        -(dimensions.pixel_height as f32) / 2.0,
        -1.0,
        1.0,
    )
    .to_arrays_transposed();

    let mut cleared = false;
    let mut draw_calls = 0usize;
    let mut vertices_total = 0usize;
    let draw_start = crate::perf::now();
    for layer in render_state.layers.borrow().iter() {
        for idx in 0..3 {
            let vb = &layer.vb.borrow()[idx];
            let (vertex_count, index_count) = vb.vertex_index_count();
            let vertex_buffer;
            let uniforms;
            if vertex_count > 0 {
                let mut vertices = vb.current_vb_mut();
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

                uniforms = webgpu.create_uniform(ShaderUniform {
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

                render_pass.set_pipeline(&webgpu.render_pipeline);
                render_pass.set_bind_group(0, &uniforms, &[]);
                render_pass.set_bind_group(1, &texture_linear_bind_group, &[]);
                render_pass.set_bind_group(2, &texture_nearest_bind_group, &[]);
                // Timed on its own because it is not the small bookkeeping step
                // it reads as: `recreate` allocates a whole new vertex buffer
                // with `mapped_at_creation`, which wgpu zero-fills. At overview
                // sizes that is megabytes per layer per frame.
                let recreate_start = crate::perf::now();
                vertex_buffer = vertices.webgpu_mut().recreate();
                crate::perf::log_duration("webgpu_vb_recreate", recreate_start);
                crate::perf::log_counter("webgpu_vb_bytes", vertices.webgpu().capacity_bytes());
                vertex_buffer.unmap();
                render_pass.set_vertex_buffer(0, vertex_buffer.slice(..));
                render_pass
                    .set_index_buffer(vb.indices.webgpu().slice(..), wgpu::IndexFormat::Uint32);
                render_pass.draw_indexed(0..index_count as _, 0, 0..1);
                draw_calls += 1;
                vertices_total += vertex_count;
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
        )
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
