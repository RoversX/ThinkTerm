package com.roversx.thinkterm

import android.content.Context
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalContentColor
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathFillType
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.PathParser
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/// The desktop's icons (third_party/lucide, as wezterm-gui's icons.rs names
/// them), drawn from the same SVG files: build.sh copies the ones the app
/// uses into assets/lucide, and they are read here as vectors once each.
object DesktopIcons {
    private val cache = HashMap<String, ImageVector?>()

    fun vector(context: Context, name: String): ImageVector? = load(context, "lucide/$name.svg", filled = false)

    /// A maker's mark (third_party/simple-icons, which build.sh copies into
    /// assets/brands): one filled shape, where Lucide's are strokes.
    fun brand(context: Context, name: String): ImageVector? = load(context, "brands/$name.svg", filled = true)

    private fun load(context: Context, path: String, filled: Boolean): ImageVector? = synchronized(cache) {
        cache.getOrPut(path) {
            runCatching {
                context.assets.open(path).bufferedReader().use { parse(path, it.readText(), filled) }
            }.getOrNull()
        }
    }

    private val element = Regex("""<(path|circle|ellipse|rect|line|polyline|polygon)\b([^>]*)/?>""")
    // Names may hold digits: a line's ends are `x1`, `y2`.
    private val attribute = Regex("""([a-zA-Z][a-zA-Z0-9-]*)="([^"]*)"""")
    private val viewBox = Regex("""viewBox="([^"]+)"""")

    /// Lucide's icons are strokes two units wide on a 24-unit square,
    /// round at their ends and corners; every shape becomes a path. A
    /// mark's shapes are filled instead, on the square its own `viewBox`
    /// says, holes where its fill rule makes them; a shade drawn in a
    /// gradient is left out, the mark being one colour.
    private fun parse(name: String, svg: String, filled: Boolean): ImageVector {
        val box = viewBox.find(svg)?.groupValues?.get(1)?.trim()?.split(Regex("[\\s,]+"))
            ?.mapNotNull { it.toFloatOrNull() }?.takeIf { it.size == 4 && it[2] > 0f && it[3] > 0f }
            ?: listOf(0f, 0f, 24f, 24f)
        val builder = ImageVector.Builder(
            name = name,
            defaultWidth = (24f * box[2] / box[3]).dp,
            defaultHeight = 24.dp,
            viewportWidth = box[2],
            viewportHeight = box[3],
        )
        builder.addGroup(translationX = -box[0], translationY = -box[1])
        val fillType = if (svg.contains("fill-rule=\"evenodd\"")) PathFillType.EvenOdd else PathFillType.NonZero
        for (match in element.findAll(svg)) {
            val attrs = attribute.findAll(match.groupValues[2]).associate { it.groupValues[1] to it.groupValues[2] }
            if (attrs["fill"]?.startsWith("url(") == true) continue
            val d = pathData(match.groupValues[1], attrs) ?: continue
            if (filled) {
                builder.addPath(
                    pathData = PathParser().parsePathString(d).toNodes(),
                    fill = SolidColor(Color.Black),
                    pathFillType = fillType,
                )
            } else {
                builder.addPath(
                    pathData = PathParser().parsePathString(d).toNodes(),
                    fill = null,
                    stroke = SolidColor(Color.Black),
                    strokeLineWidth = 2f,
                    strokeLineCap = StrokeCap.Round,
                    strokeLineJoin = StrokeJoin.Round,
                )
            }
        }
        builder.clearGroup()
        return builder.build()
    }

    private fun pathData(kind: String, a: Map<String, String>): String? {
        fun n(key: String, default: Float = 0f) = a[key]?.toFloatOrNull() ?: default
        return when (kind) {
            "path" -> a["d"]
            "circle" -> ellipse(n("cx"), n("cy"), n("r"), n("r"))
            "ellipse" -> ellipse(n("cx"), n("cy"), n("rx"), n("ry"))
            "line" -> "M${n("x1")},${n("y1")}L${n("x2")},${n("y2")}"
            "polyline", "polygon" -> {
                val p = a["points"]?.trim()?.split(Regex("[\\s,]+"))?.mapNotNull { it.toFloatOrNull() } ?: return null
                if (p.size < 4) return null
                val pairs = p.chunked(2).filter { it.size == 2 }
                "M${pairs[0][0]},${pairs[0][1]}" + pairs.drop(1).joinToString("") { "L${it[0]},${it[1]}" } +
                    if (kind == "polygon") "Z" else ""
            }
            "rect" -> {
                val x = n("x"); val y = n("y"); val w = n("width"); val h = n("height")
                val rx = (a["rx"] ?: a["ry"])?.toFloatOrNull()?.coerceAtMost(w / 2) ?: 0f
                val ry = (a["ry"] ?: a["rx"])?.toFloatOrNull()?.coerceAtMost(h / 2) ?: 0f
                if (rx <= 0f || ry <= 0f) {
                    "M$x,${y}h${w}v${h}h${-w}Z"
                } else {
                    "M${x + rx},${y}h${w - 2 * rx}a$rx,$ry 0 0 1 $rx,$ry" +
                        "v${h - 2 * ry}a$rx,$ry 0 0 1 ${-rx},$ry" +
                        "h${-(w - 2 * rx)}a$rx,$ry 0 0 1 ${-rx},${-ry}" +
                        "v${-(h - 2 * ry)}a$rx,$ry 0 0 1 $rx,${-ry}Z"
                }
            }
            else -> null
        }
    }

    private fun ellipse(cx: Float, cy: Float, rx: Float, ry: Float) =
        "M${cx - rx},${cy}a$rx,$ry 0 1,0 ${2 * rx},0a$rx,$ry 0 1,0 ${-2 * rx},0Z"
}

/// One of the desktop's icons in the content colour, as iOS's DesktopIcon.
@Composable
fun DesktopIcon(name: String, modifier: Modifier = Modifier, size: Dp = 20.dp, tint: Color = LocalContentColor.current) {
    val context = LocalContext.current
    val vector = remember(name) { DesktopIcons.vector(context, name) } ?: return
    Icon(vector, contentDescription = null, tint = tint, modifier = modifier.size(size))
}

/// The maker's mark at the foot of About (assets/icon/CloseX.svg, which
/// build.sh copies in with the brands), `height` tall and as wide as it is,
/// in one colour.
@Composable
fun MakerLogo(height: Dp, tint: Color, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val vector = remember { DesktopIcons.brand(context, "closex") } ?: return
    Icon(
        vector,
        contentDescription = "CloseX",
        tint = tint,
        modifier = modifier.height(height).width(height * (vector.viewportWidth / vector.viewportHeight)),
    )
}

/// A maker's mark (`DesktopIcons.brand`) in one colour, as iOS's BrandIcon.
@Composable
fun BrandIcon(name: String, modifier: Modifier = Modifier, size: Dp = 20.dp, tint: Color = LocalContentColor.current) {
    val context = LocalContext.current
    val vector = remember(name) { DesktopIcons.brand(context, name) } ?: return
    Icon(vector, contentDescription = null, tint = tint, modifier = modifier.size(size))
}
