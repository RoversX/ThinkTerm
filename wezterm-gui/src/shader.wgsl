// Vertex shader

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) tex: vec2<f32>,
    @location(2) fg_color: vec4<f32>,
    @location(3) alt_color: vec4<f32>,
    @location(4) hsv: vec3<f32>,
    @location(5) has_color: f32,
    @location(6) mix_value: f32,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) tex: vec2<f32>,
    @location(1) fg_color: vec4<f32>,
    @location(2) hsv: vec3<f32>,
    @location(3) has_color: f32,
};

// a regular monochrome text glyph
const IS_GLYPH: f32 = 0.0;

// a color emoji glyph
const IS_COLOR_EMOJI: f32 = 1.0;

// a full color texture attached as the
// background image of the window
const IS_BG_IMAGE: f32 = 2.0;

// like 2.0, except that instead of an
// image, we use the solid bg color
const IS_SOLID_COLOR: f32 = 3.0;

// Grayscale poly quad for non-aa text render layers
const IS_GRAY_SCALE: f32 = 4.0;

struct ShaderUniform {
  foreground_text_hsb: vec3<f32>,
  milliseconds: u32,
  viewport_and_corner: vec4<f32>,
  window_border: vec4<f32>,
  projection: mat4x4<f32>,
};
@group(0) @binding(0) var<uniform> uniforms: ShaderUniform;

@group(1) @binding(0) var atlas_linear_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_linear_sampler: sampler;

@group(2) @binding(0) var atlas_nearest_tex: texture_2d<f32>;
@group(2) @binding(1) var atlas_nearest_sampler: sampler;

fn rgb2hsv(c: vec3<f32>) -> vec3<f32>
{
    let K = vec4<f32>(0.0, -1.0 / 3.0, 2.0 / 3.0, -1.0);
    let p = mix(vec4<f32>(c.bg, K.wz), vec4<f32>(c.gb, K.xy), step(c.b, c.g));
    let q = mix(vec4<f32>(p.xyw, c.r), vec4<f32>(c.r, p.yzx), step(p.x, c.r));

    let d = q.x - min(q.w, q.y);
    let e = 1.0e-10;
    return vec3<f32>(abs(q.z + (q.w - q.y) / (6.0 * d + e)), d / (q.x + e), q.x);
}

fn hsv2rgb(c: vec3<f32>) -> vec3<f32>
{
    let K = vec4<f32>(1.0, 2.0 / 3.0, 1.0 / 3.0, 3.0);
    let p = abs(fract(c.xxx + K.xyz) * 6.0 - K.www);
    return c.z * mix(K.xxx, clamp(p - K.xxx, vec3(0.0), vec3(1.0)), c.y);
}

fn apply_hsv(c: vec4<f32>, transform: vec3<f32>) -> vec4<f32>
{
  let hsv = rgb2hsv(c.rgb) * transform;
  return vec4<f32>(hsv2rgb(hsv).rgb, c.a);
}

fn rounded_window_coverage(position: vec2<f32>) -> f32 {
  let viewport = uniforms.viewport_and_corner.xy;
  let radius = uniforms.viewport_and_corner.z;
  if radius <= 0.0 {
    return 1.0;
  }

  let in_corner_x = position.x < radius || position.x > viewport.x - radius;
  let in_corner_y = position.y < radius || position.y > viewport.y - radius;
  if !in_corner_x || !in_corner_y {
    return 1.0;
  }

  let center = vec2<f32>(
    select(radius, viewport.x - radius, position.x > viewport.x * 0.5),
    select(radius, viewport.y - radius, position.y > viewport.y * 0.5),
  );
  let feather = 1.0;
  return 1.0 - smoothstep(radius - feather, radius, distance(position, center));
}

fn rounded_window_signed_distance(position: vec2<f32>) -> f32 {
  let viewport = uniforms.viewport_and_corner.xy;
  let half_size = viewport * 0.5;
  let radius = min(
    max(uniforms.viewport_and_corner.z, 0.0),
    min(half_size.x, half_size.y),
  );
  let q = abs(position - half_size) - (half_size - vec2<f32>(radius));
  return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - radius;
}

fn rounded_window_border_coverage(position: vec2<f32>) -> f32 {
  let width = uniforms.window_border.w;
  if width <= 0.0 {
    return 0.0;
  }
  let signed_distance = rounded_window_signed_distance(position);
  return smoothstep(
    -width - 0.5,
    -width + 0.5,
    signed_distance,
  );
}

@vertex
fn vs_main(
    model: VertexInput,
) -> VertexOutput {
    var out: VertexOutput;
    out.tex = model.tex;
    out.hsv = model.hsv;
    out.has_color = model.has_color;
    out.fg_color = mix(model.fg_color, model.alt_color, model.mix_value);
    out.clip_position = uniforms.projection * vec4<f32>(model.position, 0.0, 1.0);
    return out;
}

// Fragment shader

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
  var color: vec4<f32>;
  var linear_tex: vec4<f32> = textureSample(atlas_linear_tex, atlas_linear_sampler, in.tex);
  var nearest_tex: vec4<f32> = textureSample(atlas_nearest_tex, atlas_nearest_sampler, in.tex);

  var hsv = in.hsv;

  if in.has_color == IS_SOLID_COLOR {
    // Solid color block
    color = in.fg_color;
  } else if in.has_color == IS_BG_IMAGE {
    // Window background attachment
    // Apply window_background_image_opacity to the background image
    color = linear_tex;
    color.a *= in.fg_color.a;
  } else if in.has_color == IS_COLOR_EMOJI {
    // the texture is full color info (eg: color emoji glyph)
    color = nearest_tex;
  } else if in.has_color == IS_GRAY_SCALE {
    // Grayscale poly quad for non-aa text render layers
    color = in.fg_color;
    color.a *= nearest_tex.a;
  } else if in.has_color == IS_GLYPH {
    // the texture is the alpha channel/color mask
    // and we need to tint with the fg_color
    color = in.fg_color;
    color.a = nearest_tex.a;
    hsv *= uniforms.foreground_text_hsb;
  }

  color = apply_hsv(color, hsv);
  let border_coverage = rounded_window_border_coverage(in.clip_position.xy);
  color = mix(
    color,
    vec4<f32>(uniforms.window_border.rgb, 1.0),
    border_coverage,
  );
  let coverage = rounded_window_coverage(in.clip_position.xy);
  if coverage <= 0.0 {
    discard;
  }
  color.a *= coverage;

  return color;
}
