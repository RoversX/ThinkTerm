package com.roversx.thinkterm

import android.content.Context
import android.util.Log
import androidx.compose.ui.graphics.Color
import org.json.JSONArray

/// The colour schemes in assets/schemes.json (the web page's list,
/// straight from the desktop's), by name. Read once, on first use.
object Schemes {
    const val FOLLOW_DESKTOP = "desktop"

    /// Every scheme's name, in the file's order (alphabetical).
    var names: List<String> = emptyList()
        private set

    private var byName: Map<String, String> = emptyMap()
    private var backgrounds: Map<String, Color> = emptyMap()
    private var foregrounds: Map<String, Color> = emptyMap()

    fun load(context: Context) {
        if (names.isNotEmpty()) return
        try {
            val text = context.assets.open("schemes.json").bufferedReader().use { it.readText() }
            val list = JSONArray(text)
            val names = ArrayList<String>(list.length())
            val json = HashMap<String, String>()
            val bg = HashMap<String, Color>()
            val fg = HashMap<String, Color>()
            for (i in 0 until list.length()) {
                val entry = list.optJSONObject(i) ?: continue
                val name = entry.optString("name").ifEmpty { continue }
                names.add(name)
                json[name] = entry.toString()
                parseHex(entry.optString("background"))?.let { bg[name] = it }
                parseHex(entry.optString("foreground"))?.let { fg[name] = it }
            }
            this.names = names
            byName = json
            backgrounds = bg
            foregrounds = fg
        } catch (e: Throwable) {
            Log.w("thinkterm", "schemes.json unreadable: $e")
        }
    }

    /// One scheme as the core takes it: its JSON object, unchanged.
    fun json(name: String): String? = byName[name]

    /// The background of a scheme, for the picker's swatches.
    fun background(name: String): Color? = backgrounds[name]

    /// A scheme's own foreground, so "Aa" shows on a light swatch too.
    fun foreground(name: String): Color? = foregrounds[name]
}

/// "#rrggbb", "#rgb" or "#rrggbbaa"; null for anything else.
fun parseHex(value: String): Color? {
    var s = value.trim().removePrefix("#")
    if (s.length == 3) s = s.map { "$it$it" }.joinToString("")
    return try {
        when (s.length) {
            6 -> Color(0xFF000000L or s.toLong(16))
            8 -> Color(s.toLong(16))
            else -> null
        }
    } catch (e: Throwable) {
        null
    }
}
