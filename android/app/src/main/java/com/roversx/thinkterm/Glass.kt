package com.roversx.thinkterm

import android.graphics.RenderEffect
import androidx.compose.foundation.background
import androidx.compose.foundation.shape.CornerBasedShape
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.graphics.compositeOver
import androidx.compose.ui.unit.dp
import com.kyant.backdrop.Backdrop
import com.kyant.backdrop.drawBackdrop
import com.kyant.backdrop.drawPlainBackdrop
import com.kyant.backdrop.effects.effect
import com.kyant.backdrop.effects.blur
import com.kyant.backdrop.effects.lens
import com.kyant.backdrop.effects.vibrancy

/// Liquid glass, from the Backdrop library: what the terminal draws shows
/// through every glass surface over it, blurred a little and bent at the
/// edges, as the iOS app's glass does. The terminal's view records itself
/// into this backdrop; null where there is no terminal under the chrome.
val LocalGlass = staticCompositionLocalOf<Backdrop?> { null }

/// What a list's lit row (the sidebar's thread on show) is glass over: the
/// list's ground, a layer of its own behind the list. Null for a plain fill.
val LocalRowGlass = staticCompositionLocalOf<Backdrop?> { null }

/// A glass surface in `shape`. Its wash follows the lightness of what it
/// sits on, so its ink stays readable over a busy terminal; `tint` washes
/// it in a colour instead, for the one button that leads. With nothing to
/// sample it is a plain fill.
fun Modifier.glass(backdrop: Backdrop?, dark: Boolean, shape: CornerBasedShape, tint: Color? = null): Modifier {
    // Enough wash and blur that what passes under it never reads through
    // the button's own label.
    // A tint already see-through is a lit piece's (a tab on show), laid
    // over the usual wash; an opaque one leads (New Thread).
    val base = if (dark) Color.Black.copy(alpha = 0.38f) else Color.White.copy(alpha = 0.6f)
    val wash = when {
        tint == null -> base
        tint.alpha < 1f -> tint.compositeOver(base)
        else -> tint.copy(alpha = 0.92f)
    }
    if (backdrop == null) {
        return clip(shape).background(tint ?: if (dark) Color(0xF02C2C2E) else Color(0xF0E5E5EA))
    }
    return drawBackdrop(
        backdrop = backdrop,
        shape = { shape },
        effects = {
            vibrancy()
            blur(8.dp.toPx())
            lens(12.dp.toPx(), 24.dp.toPx())
        },
        onDrawSurface = { drawRect(wash) },
    ).clip(shape)
}

/// iOS's soft scroll edge: what passes under a bar blurs, and fades into
/// the ground, the more the nearer it comes to the screen's edge -- `top`
/// for a bar at the top, else at the bottom. Drawn from `backdrop`, the
/// list that scrolls under it, which must not hold the bar itself.
fun Modifier.progressiveBlur(backdrop: Backdrop, ground: Color, top: Boolean): Modifier = drawPlainBackdrop(
    backdrop = backdrop,
    shape = { RectangleShape },
    effects = {
        blur(8.dp.toPx())
        val shader = obtainRuntimeShader(if (top) "SoftEdgeTop" else "SoftEdgeBottom", softEdge(top)).apply {
            setFloatUniform("size", size.width, size.height)
            setColorUniform("tint", ground.toArgb())
        }
        effect(RenderEffect.createRuntimeShaderEffect(shader, "content"))
    },
)

/// The edge's mask: the blur whole near the screen's edge and gone at the
/// bar's far side; over it the ground, strongest at the edge.
private fun softEdge(top: Boolean): String = """
uniform shader content;
uniform float2 size;
layout(color) uniform half4 tint;

half4 main(float2 coord) {
    float t = ${if (top) "coord.y / size.y" else "1.0 - coord.y / size.y"};
    float blurAlpha = 1.0 - smoothstep(0.5, 1.0, t);
    float tintAlpha = (1.0 - smoothstep(0.0, 0.85, t)) * 0.8;
    half4 blurred = content.eval(coord) * blurAlpha;
    return mix(blurred, tint * blurAlpha, tintAlpha);
}"""

/// A debug build's glass capsule kept over the terminal, to see the glass
/// and time its frames: `am start ... --ez glass_probe true`. Release
/// builds never read the flag.
object GlassProbe {
    var on = false
}
