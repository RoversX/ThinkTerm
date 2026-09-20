package com.roversx.thinkterm

import android.content.Context
import android.content.SharedPreferences
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

/// A command the user saved. Tapped in the key panel it goes to the pane
/// as typed text; `runs` adds the Enter that runs it.
data class Snippet(val id: String = UUID.randomUUID().toString(), val text: String, val runs: Boolean = true)

/// The saved snippets, as JSON in the preferences under "keypanel.snippets".
/// A handful of short strings: kept whole in memory, rewritten on change.
class SnippetStore private constructor(private val prefs: SharedPreferences) {
    companion object {
        private const val KEY = "keypanel.snippets"
        @Volatile private var instance: SnippetStore? = null

        fun get(context: Context): SnippetStore = instance ?: synchronized(this) {
            instance ?: SnippetStore(
                context.applicationContext.getSharedPreferences("thinkterm-settings", Context.MODE_PRIVATE)
            ).also { instance = it }
        }

        /// What a fresh install starts with, from the prototype's list.
        private val starter = listOf(
            Snippet(text = "git status -sb"),
            Snippet(text = "thinkterm cli --prefer-mux list"),
            Snippet(text = "journalctl -u thinkterm-mux -f"),
            Snippet(text = "btop"),
        )
    }

    var items: List<Snippet> by mutableStateOf(load())
        private set

    fun add(text: String, runs: Boolean) {
        val clean = text.trim()
        if (clean.isEmpty()) return
        items = items + Snippet(text = clean, runs = runs)
        save()
    }

    fun replace(snippet: Snippet) {
        val at = items.indexOfFirst { it.id == snippet.id }
        if (at < 0) return
        val clean = snippet.text.trim()
        items = if (clean.isEmpty()) items.filterIndexed { i, _ -> i != at }
        else items.toMutableList().also { it[at] = snippet.copy(text = clean) }
        save()
    }

    fun remove(snippet: Snippet) {
        items = items.filter { it.id != snippet.id }
        save()
    }

    private fun load(): List<Snippet> {
        val json = prefs.getString(KEY, null) ?: return starter
        return try {
            val arr = JSONArray(json)
            (0 until arr.length()).mapNotNull { i ->
                val o = arr.optJSONObject(i) ?: return@mapNotNull null
                Snippet(o.optString("id", UUID.randomUUID().toString()), o.optString("text"), o.optBoolean("runs", true))
            }
        } catch (e: Throwable) {
            starter
        }
    }

    private fun save() {
        val arr = JSONArray()
        for (s in items) arr.put(JSONObject().put("id", s.id).put("text", s.text).put("runs", s.runs))
        prefs.edit().putString(KEY, arr.toString()).apply()
    }
}

/// The last commands the key panel sent, newest first. Only what went
/// through the key bar and the panel; it outlives the app in the
/// preferences.
class KeyHistory private constructor(private val prefs: SharedPreferences) {
    companion object {
        private const val KEY = "keypanel.history"
        private const val LIMIT = 50
        @Volatile private var instance: KeyHistory? = null

        fun get(context: Context): KeyHistory = instance ?: synchronized(this) {
            instance ?: KeyHistory(
                context.applicationContext.getSharedPreferences("thinkterm-settings", Context.MODE_PRIVATE)
            ).also { instance = it }
        }
    }

    var lines: List<String> by mutableStateOf(load())
        private set

    fun record(text: String) {
        val line = text.trim()
        // A lone bracket or pipe from the key grid is not worth keeping;
        // a snippet or a pasted command is.
        if (line.length <= 1) return
        lines = (listOf(line) + lines.filter { it != line }).take(LIMIT)
        save()
    }

    fun clear() {
        lines = emptyList()
        prefs.edit().remove(KEY).apply()
    }

    private fun load(): List<String> {
        val json = prefs.getString(KEY, null) ?: return emptyList()
        return try {
            val arr = JSONArray(json)
            (0 until arr.length()).map { arr.optString(it) }.filter { it.isNotEmpty() }
        } catch (e: Throwable) {
            emptyList()
        }
    }

    private fun save() {
        prefs.edit().putString(KEY, JSONArray(lines).toString()).apply()
    }
}
