package com.roversx.thinkterm

import android.content.Context
import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import org.json.JSONObject
import java.util.Locale

/// The app's text in five languages, from assets/strings.json (the same
/// tables the iOS app ships; ios/tools/gen-strings.py builds them).
/// English stands in for a key a table has not got.
object L10n {
    private const val FALLBACK = "en-US"

    /// language tag -> key -> text.
    private var tables: Map<String, Map<String, String>> = emptyMap()

    fun load(context: Context) {
        if (tables.isNotEmpty()) return
        tables = try {
            val text = context.assets.open("strings.json").bufferedReader().use { it.readText() }
            val root = JSONObject(text)
            root.keys().asSequence().associateWith { tag ->
                val table = root.getJSONObject(tag)
                table.keys().asSequence().associateWith { table.getString(it) }
            }
        } catch (e: Throwable) {
            Log.w("thinkterm", "strings.json is missing: the app shows its keys ($e)")
            emptyMap()
        }
    }

    /// The tags with a table, in the order of the settings' picker.
    val tags: List<String> get() = listOf("en-US", "zh-CN", "ja-JP", "fr-FR", "de-DE").filter { tables.containsKey(it) }

    /// The phone's own language, matched against the tables.
    val systemTag: String
        get() {
            val locale = Locale.getDefault()
            return match(locale.toLanguageTag()) ?: match(locale.language) ?: FALLBACK
        }

    /// "zh-Hans-CN" and "zh" both find the "zh-CN" table.
    private fun match(language: String): String? {
        if (tables.containsKey(language)) return language
        val code = language.substringBefore('-')
        return tables.keys.sorted().firstOrNull { it.substringBefore('-') == code }
    }

    /// The tag in force: the phone's own when the setting is "system".
    fun currentTag(setting: String): String = if (setting == "system") systemTag else (match(setting) ?: FALLBACK)

    fun text(key: String, setting: String): String {
        val tag = currentTag(setting)
        return tables[tag]?.get(key) ?: tables[FALLBACK]?.get(key) ?: key
    }
}

/// What the views follow to relabel themselves: reading `tag` in a
/// composable subscribes it, so a change in the settings relabels the
/// screen without a relaunch. Main thread only.
object AppLanguage {
    var tag: String by mutableStateOf("system")
}

/// One string by key; extra arguments fill its %@ and %d, the iOS way.
fun tr(key: String, vararg args: Any): String {
    val format = L10n.text(key, AppLanguage.tag)
    if (args.isEmpty()) return format
    var out = format
    for (arg in args) {
        val at = out.indexOfAny(listOf("%@", "%d", "%s"))
        if (at < 0) break
        out = out.substring(0, at) + arg.toString() + out.substring(at + 2)
    }
    return out
}
