import Metal

/// The starry sky of `LaunchSky`, made pixel by pixel on the GPU so each
/// star is as sharp as the screen allows: a core a pixel or two across in
/// a soft glow, the brightest with faint diffraction spikes. Four layers
/// at their own depths, so nearer stars move past farther ones; going in,
/// each comes nearer and, once past, starts again far off with other
/// stars, so the sky never empties however deep it goes. Colour is
/// a star's temperature, as a black body glows -- most near white, some
/// red, some blue -- shifted toward blue ahead and red around when the sky
/// rushes past, as light is. A faint band of the galaxy with its dust runs
/// across. Android's `SKY_SHADER` is the same sky, line for line.
///
/// Compiled when first wanted, by the system's Metal: a `.metal` file
/// would need Xcode's separate Metal toolchain to build the app at all.
/// Lengths are in points (`scale` pixels each).
enum SkyShader {
    static let source = """
#include <metal_stdlib>
using namespace metal;

struct SkyIn {
    float sizeX, sizeY, scale, time, drift, camera, cameraPrev, rush, rushPrev, fade, bg, still;
};

vertex float4 skyVertex(uint vid [[vertex_id]]) {
    float2 corners[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };
    return float4(corners[vid], 0.0, 1.0);
}

static float hash21(float2 p) {
    p = fract(p * float2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
}

static float2 hash22(float2 p) {
    float n = hash21(p);
    return float2(n, hash21(p + n + 19.19));
}

static float vnoise(float2 p) {
    float2 i = floor(p);
    float2 f = fract(p);
    float2 u = f * f * (3.0 - 2.0 * f);
    float a = hash21(i);
    float b = hash21(i + float2(1.0, 0.0));
    float c = hash21(i + float2(0.0, 1.0));
    float d = hash21(i + float2(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

static float fbm(float2 p) {
    float v = 0.0;
    float a = 0.5;
    for (int i = 0; i < 4; i++) {
        v += a * vnoise(p);
        p = p * 2.03 + float2(17.1, 9.2);
        a *= 0.5;
    }
    return v;
}

// A black body's colour at `k` kelvin (Tanner Helland's fit).
static float3 blackbody(float k) {
    float t = k / 100.0;
    float r = t <= 66.0 ? 1.0 : clamp(1.292936 * pow(t - 60.0, -0.1332048), 0.0, 1.0);
    float g = t <= 66.0 ? clamp(0.390082 * log(t) - 0.631841, 0.0, 1.0)
                        : clamp(1.129891 * pow(t - 60.0, -0.0755148), 0.0, 1.0);
    float b = t >= 66.0 ? 1.0 : (t <= 19.0 ? 0.0 : clamp(0.543207 * log(t - 10.0) - 1.196254, 0.0, 1.0));
    return float3(r, g, b);
}

// The band of the galaxy across the screen, 0 to 1, at `q` (by height).
static float galaxy(float2 q, float aspect) {
    float2 n = float2(-0.78, 0.62);
    float d = dot(q - float2(0.5 * aspect, 0.5), n);
    return exp(-d * d / 0.035);
}

// Where a point of the sky shows on screen: drifting down slowly, and
// zoomed toward the viewer by depth about the middle of the screen.
static float2 toScreen(float2 a, float z, float zoom, float2 c, float time) {
    float2 w = a + float2(0.0, time * 3.0 * z);
    return c + (w - c) * zoom;
}

static float2 toSky(float2 p, float z, float zoom, float2 c, float time) {
    float2 w = c + (p - c) / zoom;
    return w - float2(0.0, time * 3.0 * z);
}

fragment half4 skyFragment(float4 frag [[position]], constant SkyIn& u [[buffer(0)]]) {
    float2 fragCoord = frag.xy;
    float time = u.time;
    float2 view = float2(u.sizeX, u.sizeY) / u.scale;
    float2 c = view * 0.5;
    float2 p = fragCoord / u.scale;
    float aspect = view.x / view.y;
    float moving = u.still > 0.5 ? 0.0 : clamp(max(u.rush - u.rushPrev, 0.0) * 1.5 + abs(u.camera - u.cameraPrev) * 2.0, 0.0, 1.0);
    float3 light = float3(0.0);
    // How far into the sky the view has come, in layers: the slow drift
    // at the start, each page deeper, and the rush.
    float depth = u.drift * 0.4 + u.camera * 0.35 + u.rush * 0.6;
    float depthPrev = u.drift * 0.4 + u.cameraPrev * 0.35 + u.rushPrev * 0.6;
    for (int l = 0; l < 4; l++) {
        float fl = float(l);
        // Each layer comes nearer as the view goes in, from far and faint
        // to near and bright; past the viewer it starts again far off with
        // other stars, so the sky never empties however deep it goes.
        float f1 = fl * 0.25 + depth;
        float f0 = fl * 0.25 + depthPrev;
        float round1 = floor(f1);
        float near = fract(f1);
        float nearPrev = floor(f0) == round1 ? fract(f0) : near;
        float zoom = exp2(near * 3.0);
        float zoomPrev = exp2(nearPrev * 3.0);
        float seen = smoothstep(0.0, 0.2, near) * (1.0 - smoothstep(0.8, 1.0, near));
        float cell = 15.0;
        float bMax = 0.5 + near * 1.6;
        float2 a1 = toSky(p, 0.25, zoom, c, time);
        float2 a0 = toSky(p, 0.25, zoomPrev, c, time);
        float2 g = floor((a0 + a1) * 0.5 / cell);
        for (int j = -1; j <= 1; j++) {
            for (int i = -1; i <= 1; i++) {
                float2 cellId = g + float2(float(i), float(j));
                float2 id = cellId + (fl + round1 * 4.0) * 101.0;
                if (hash21(id) > 0.5) continue;
                float2 anchor = (cellId + 0.12 + 0.76 * hash22(id * 1.31)) * cell;
                float2 s1 = toScreen(anchor, 0.25, zoom, c, time);
                float2 s0 = toScreen(anchor, 0.25, zoomPrev, c, time);
                // Most stars faint, a few bright; the galaxy's crowd of
                // faint ones far off.
                float b = bMax * (0.2 + 0.8 * pow(hash21(id * 2.17 + 3.1), 3.0 + near * 2.4));
                b *= 1.0 + 1.6 * galaxy(s1 / view.y, aspect) * (1.0 - smoothstep(0.3, 0.55, near));
                // In motion a star is the stretch it crossed, brightest at
                // its head and thinner the longer it is.
                float2 sv = s1 - s0;
                float len2 = dot(sv, sv);
                float along = len2 > 0.000001 ? clamp(dot(p - s0, sv) / len2, 0.0, 1.0) : 1.0;
                float2 d = (p - (s0 + sv * along)) * u.scale;
                float r2 = dot(d, d);
                if (r2 > 2500.0) continue;
                float r = sqrt(r2);
                float lenPx = sqrt(len2) * u.scale;
                float spread = 1.0 / (1.0 + lenPx * 0.08);
                float head = mix(0.35, 1.0, along);
                float sigma = 0.55 + 0.35 * b;
                float core = exp(-r2 / (2.0 * sigma * sigma));
                // The glow ends well before the edge of what is looked at,
                // so no star has a rim to its halo.
                float reach = smoothstep(2500.0, 400.0, r2);
                float glow = (0.09 * b * exp(-r / (1.0 + 2.6 * b)) + 0.022 * b * exp(-r / (4.0 + 6.0 * b))) * reach;
                // Spikes, as a lens makes them, on the brightest few only.
                float spikes = 0.0;
                if (b > 1.45 && lenPx < 2.0) {
                    float sl = 4.0 + 6.0 * b;
                    spikes = (b - 1.45) * 0.22 * (exp(-abs(d.x) / 0.45) * exp(-abs(d.y) / sl) + exp(-abs(d.y) / 0.45) * exp(-abs(d.x) / sl)) * reach;
                }
                // The air's shimmer: a few rates at once, the bright ones most.
                float ph = hash21(id * 3.7) * 6.2831;
                float f = 1.4 + 2.2 * hash21(id * 5.3);
                float amp = 0.18 + 0.22 * min(b, 1.0);
                float twinkle = 1.0 + amp * (0.55 * sin(time * f + ph) + 0.3 * sin(time * f * 2.37 + ph * 2.0) + 0.15 * sin(time * f * 4.13 + ph * 3.0));
                float bornAt = hash21(id * 7.1) * 0.5;
                float born = smoothstep(bornAt, bornAt + 0.35, time);
                // Temperature: most between orange and white, a few red, a
                // few blue-white; bluer ahead and redder around in motion.
                float ht = hash21(id * 4.9);
                float k = ht < 0.06 ? 2400.0 + ht / 0.06 * 900.0
                        : (ht > 0.93 ? 12000.0 + (ht - 0.93) / 0.07 * 16000.0
                        : 3600.0 + 7000.0 * pow((ht - 0.06) / 0.87, 1.3));
                float rn = length(s1 - c) / length(c);
                k *= 1.0 + moving * 0.9 * (0.55 - rn);
                float3 col = mix(float3(1.0), blackbody(clamp(k, 1500.0, 40000.0)), 0.62);
                col /= max(max(col.r, col.g), col.b);
                light += col * ((core * head + glow) * spread + spikes) * b * twinkle * born * seen;
            }
        }
    }
    light = 1.0 - exp(-light * 3.0);
    // The galaxy's glow, its dust lanes darker, and the faintest haze.
    float2 q = p / view.y;
    float band = galaxy(q, aspect);
    float n = fbm(q * 3.0 + float2(time * 0.01, 0.0));
    float n2 = fbm(q * 7.0 - 3.1);
    float3 neb = mix(float3(0.06, 0.08, 0.20), float3(0.16, 0.07, 0.20), n2) * (0.25 + n) * band * 0.22;
    neb *= mix(1.0, smoothstep(0.35, 0.65, n2), band * 0.6);
    neb += float3(0.012, 0.016, 0.034) * n;
    float3 ground = float3(0.004, 0.005, 0.014) + neb;
    float3 rgb = ground * u.bg + light * u.fade;
    float a = clamp(u.bg + max(rgb.r, max(rgb.g, rgb.b)) * (1.0 - u.bg), 0.0, 1.0);
    return half4(half3(rgb), half(a));
}
"""
}

import MetalKit
import SwiftUI

/// What the sky is drawn with for a frame (`SkyIn` in the shader).
struct SkyUniforms {
    var sizeX: Float = 0, sizeY: Float = 0, scale: Float = 1, time: Float = 0
    var drift: Float = 0, camera: Float = 0, cameraPrev: Float = 0
    var rush: Float = 0, rushPrev: Float = 0, fade: Float = 1, bg: Float = 1, still: Float = 0
}

/// The sky on a Metal view of its own, at sixty frames a second, its
/// colours over what is behind it (the app, as it comes out). Plain black
/// at `bg` where the shader will not build.
struct SkyMetal: UIViewRepresentable {
    /// The frame's uniforms, asked for as each one is drawn.
    var uniforms: (CGSize, CGFloat) -> SkyUniforms

    func makeCoordinator() -> Renderer { Renderer() }

    func makeUIView(context: Context) -> MTKView {
        let view = MTKView(frame: .zero, device: Renderer.device)
        view.isOpaque = false
        view.backgroundColor = .clear
        view.clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 0)
        view.colorPixelFormat = .bgra8Unorm
        view.framebufferOnly = true
        view.preferredFramesPerSecond = 60
        view.isUserInteractionEnabled = false
        context.coordinator.uniforms = uniforms
        view.delegate = context.coordinator
        return view
    }

    func updateUIView(_ view: MTKView, context: Context) {
        context.coordinator.uniforms = uniforms
    }

    final class Renderer: NSObject, MTKViewDelegate {
        static let device = MTLCreateSystemDefaultDevice()
        /// Built once, the first time a sky is shown.
        static let pipeline: MTLRenderPipelineState? = {
            guard let device else { return nil }
            do {
                let library = try device.makeLibrary(source: SkyShader.source, options: nil)
                let describe = MTLRenderPipelineDescriptor()
                describe.vertexFunction = library.makeFunction(name: "skyVertex")
                describe.fragmentFunction = library.makeFunction(name: "skyFragment")
                describe.colorAttachments[0].pixelFormat = .bgra8Unorm
                return try device.makeRenderPipelineState(descriptor: describe)
            } catch {
                NSLog("sky shader: \(error)")
                return nil
            }
        }()
        private lazy var queue = Renderer.device?.makeCommandQueue()
        var uniforms: ((CGSize, CGFloat) -> SkyUniforms)?

        func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

        func draw(in view: MTKView) {
            guard let uniforms, let pass = view.currentRenderPassDescriptor,
                  let drawable = view.currentDrawable, let buffer = queue?.makeCommandBuffer() else { return }
            var u = uniforms(view.bounds.size, view.contentScaleFactor)
            if Self.pipeline == nil {
                // The fallback: the ground alone, black.
                pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: Double(u.bg))
            }
            pass.colorAttachments[0].loadAction = .clear
            guard let encoder = buffer.makeRenderCommandEncoder(descriptor: pass) else { return }
            if let pipeline = Self.pipeline {
                encoder.setRenderPipelineState(pipeline)
                encoder.setFragmentBytes(&u, length: MemoryLayout<SkyUniforms>.stride, index: 0)
                encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
            }
            encoder.endEncoding()
            buffer.present(drawable)
            buffer.commit()
        }
    }
}
