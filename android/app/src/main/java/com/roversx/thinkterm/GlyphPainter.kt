package com.roversx.thinkterm

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Typeface
import com.roversx.thinkterm.core.GlyphPainter

/// What the shaped fonts cannot draw (emoji, most symbols) the platform
/// draws instead: a transparent bitmap the core blits into its atlas.
/// Called on the core thread, so nothing here touches the UI.
class AndroidGlyphPainter : GlyphPainter {
    override fun paint(
        text: String,
        px: Double,
        width: UInt,
        height: UInt,
        penX: Double,
        baseline: Double,
        red: UByte,
        green: UByte,
        blue: UByte,
    ): ByteArray {
        val w = width.toInt()
        val h = height.toInt()
        if (w <= 0 || h <= 0 || text.isEmpty()) return ByteArray(0)
        return try {
            val bitmap = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
            bitmap.eraseColor(Color.TRANSPARENT)
            val paint = Paint(Paint.ANTI_ALIAS_FLAG).apply {
                typeface = Typeface.MONOSPACE
                textSize = px.toFloat()
                color = Color.rgb(red.toInt(), green.toInt(), blue.toInt())
                isSubpixelText = true
            }
            Canvas(bitmap).drawText(text, penX.toFloat(), baseline.toFloat(), paint)

            val pixels = IntArray(w * h)
            // getPixels hands back straight (un-premultiplied) ARGB ints,
            // which is exactly what the core asks for.
            bitmap.getPixels(pixels, 0, w, 0, 0, w, h)
            bitmap.recycle()
            val out = ByteArray(w * h * 4)
            var i = 0
            for (p in pixels) {
                out[i] = ((p shr 16) and 0xff).toByte()
                out[i + 1] = ((p shr 8) and 0xff).toByte()
                out[i + 2] = (p and 0xff).toByte()
                out[i + 3] = ((p ushr 24) and 0xff).toByte()
                i += 4
            }
            out
        } catch (e: Throwable) {
            ByteArray(0)
        }
    }
}
