package com.roversx.thinkterm

import android.graphics.Bitmap
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.requiredSize
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.wrapContentSize
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import com.roversx.thinkterm.core.Core
import com.roversx.thinkterm.core.threadMark
import java.nio.ByteBuffer
import kotlinx.coroutines.delay

/// What the core draws for the shell to show as it is -- the program cards
/// and the threads' marks, from the look every client shares
/// (thinkterm-tab-icons' look.rs) -- kept as bitmaps once each, as iOS's
/// TabIconImages and ThreadMarkImages keep theirs.
object CorePictures {
    /// A row's icons, in pixels: the picture, how far into it the icons'
    /// own box starts, the room a row keeps for them, and the disc for the
    /// programs past the first.
    class Cards(val image: ImageBitmap, val inset: Int, val slot: Int, val more: Int, val disc: Rect?)

    /// A mark: one picture `side` pixels square, or one for each step of a
    /// mark that moves.
    class Mark(val frames: List<ImageBitmap>, val side: Int, val stepMs: Long)

    private val cards = HashMap<String, Cards?>()
    private val marks = HashMap<String, Mark?>()
    private var revision = 0uL
    private const val CARDS_LIMIT = 256
    private const val MARKS_LIMIT = 128

    fun cards(core: Core, ids: List<String>, pixels: Int, scale: Float, dark: Boolean, glyphs: Boolean): Cards? {
        if (ids.isEmpty() || pixels <= 0) return null
        // Another host's cards, or the built-in ones again: what was drawn
        // before is stale.
        val now = core.tabIconsRevision()
        if (now != revision) {
            cards.clear()
            revision = now
        }
        val key = ids.joinToString(",") + "@$pixels" + (if (dark) "d" else "l") + (if (glyphs) "g" else "")
        if (cards.containsKey(key)) return cards[key]
        if (cards.size >= CARDS_LIMIT) cards.clear()
        val drawn = core.cardsPicture(ids, pixels.toUInt(), scale, dark, glyphs)
        val made = drawn?.let { d ->
            val image = bitmap(d.picture.rgba, d.picture.width.toInt(), d.picture.height.toInt()) ?: return@let null
            val count = d.count
            Cards(
                image = image,
                inset = d.inset.toInt(),
                slot = d.slot.toInt(),
                more = count?.more?.toInt() ?: 0,
                disc = count?.let { Rect(it.x, it.y, it.x + it.diameter, it.y + it.diameter) },
            )
        }
        cards[key] = made
        return made
    }

    fun mark(status: String, dot: String, live: Boolean, pixels: Int, dark: Boolean): Mark? {
        val key = "$status|$dot|$live|$pixels|$dark"
        if (marks.containsKey(key)) return marks[key]
        if (marks.size >= MARKS_LIMIT) marks.clear()
        val made = threadMark(status, dot, live, pixels.toUInt(), dark)?.let { d ->
            val side = d.side.toInt()
            val frames = d.frames.mapNotNull { bitmap(it, side, side) }
            if (frames.isEmpty() || frames.size != d.frames.size) null else Mark(frames, side, d.stepMs.toLong())
        }
        marks[key] = made
        return made
    }

    /// Four bytes a pixel, red first, already multiplied by their alpha:
    /// an ARGB_8888 bitmap's own bytes.
    fun bitmap(rgba: ByteArray, width: Int, height: Int): ImageBitmap? {
        if (width <= 0 || height <= 0 || rgba.size != width * height * 4) return null
        val bitmap = Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
        bitmap.copyPixelsFromBuffer(ByteBuffer.wrap(rgba))
        return bitmap.asImageBitmap()
    }

    /// The memory runs short: drawn again when next shown.
    fun trim() {
        cards.clear()
        marks.clear()
    }
}

/// What a thread runs, leading its row as the desktop's does: the first
/// program's card, the second's small at its corner, or how many more.
/// The picture's shadows fall outside the room the row keeps.
@Composable
fun ThreadCards(core: Core, icons: List<String>, size: androidx.compose.ui.unit.Dp = 30.dp, dark: Boolean) {
    val density = LocalDensity.current
    val pixels = with(density) { size.roundToPx() }
    val cards = remember(icons, pixels, dark, core.tabIconsRevision()) {
        CorePictures.cards(core, icons, pixels, density.density, dark, glyphs = true)
    } ?: return
    val slot = with(density) { cards.slot.toDp() }
    Box(Modifier.width(slot).height(size).wrapContentSize(Alignment.TopStart, unbounded = true)) {
        Image(
            cards.image,
            contentDescription = null,
            modifier = Modifier
                .offset { IntOffset(-cards.inset, -cards.inset) }
                .requiredSize(with(density) { cards.image.width.toDp() }, with(density) { cards.image.height.toDp() }),
        )
        val disc = cards.disc
        if (disc != null && cards.more > 0) {
            Box(
                Modifier
                    .offset { IntOffset((disc.left - cards.inset).toInt(), (disc.top - cards.inset).toInt()) }
                    .size(with(density) { disc.width.toDp() }),
                contentAlignment = Alignment.Center,
            ) {
                Text(
                    "${minOf(cards.more, 99)}",
                    color = Color.White,
                    fontWeight = FontWeight.Bold,
                    fontSize = with(density) { (disc.width * 0.65f).toSp() },
                    maxLines = 1,
                )
            }
        }
    }
}

/// A thread's mark as the desktop's sidebar draws it; one that moves steps
/// by the wall clock, so every mark on the screen steps together. Every
/// state takes the same square, `size` x 2, so a row keeps its width as the
/// status changes; a finished thread's tick reaches past it, as on the
/// desktop.
@Composable
fun ThreadMark(status: String, dot: String, live: Boolean, size: androidx.compose.ui.unit.Dp = 9.dp, dark: Boolean) {
    val density = LocalDensity.current
    val side = size * 2
    val pixels = with(density) { side.roundToPx() }
    val mark = remember(status, dot, live, pixels, dark) { CorePictures.mark(status, dot, live, pixels, dark) } ?: return
    var frame by remember(mark) { mutableIntStateOf(0) }
    if (mark.frames.size > 1 && mark.stepMs > 0) {
        LaunchedEffect(mark) {
            while (true) {
                val now = System.currentTimeMillis()
                frame = ((now / mark.stepMs) % mark.frames.size).toInt()
                delay(mark.stepMs - now % mark.stepMs)
            }
        }
    }
    // The picture is the mark with room round it, at its own size.
    Box(Modifier.size(side), contentAlignment = Alignment.Center) {
        Image(
            mark.frames[frame.coerceIn(0, mark.frames.size - 1)],
            contentDescription = null,
            modifier = Modifier.requiredSize(with(density) { mark.side.toDp() }),
        )
    }
}
