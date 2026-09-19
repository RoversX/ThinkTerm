package com.roversx.thinkterm

import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

/// The App's views, as its JSON names them. Only the fields this shell
/// shows are picked out; org.json is in the platform, so no codegen.

data class TabEntry(
    val tab: Int,
    val label: String,
    val title: String,
    val target: Int,
    val current: Boolean,
)

data class TabsView(val tabs: List<TabEntry>)

data class StatusView(
    val summary: String,
    val toast: String?,
    val sticky: Boolean,
)

object Views {
    fun tabs(json: String): TabsView? {
        val root = obj(json) ?: return null
        val arr = root.optJSONArray("tabs") ?: JSONArray()
        val out = ArrayList<TabEntry>(arr.length())
        for (i in 0 until arr.length()) {
            val t = arr.optJSONObject(i) ?: continue
            out.add(
                TabEntry(
                    tab = t.optInt("tab"),
                    label = t.optString("label"),
                    title = t.optString("title"),
                    target = t.optInt("target"),
                    current = t.optBoolean("current"),
                )
            )
        }
        return TabsView(out)
    }

    fun status(json: String): StatusView? {
        val root = obj(json) ?: return null
        val toast = root.optJSONObject("toast")
        return StatusView(
            summary = root.optString("summary"),
            toast = toast?.optString("text")?.ifEmpty { null },
            sticky = toast?.optBoolean("sticky") ?: false,
        )
    }

    private fun obj(json: String): JSONObject? {
        if (json.isEmpty() || json == "null") return null
        return try {
            JSONObject(json)
        } catch (e: Throwable) {
            Log.w("thinkterm", "view decode failed: ${json.take(200)}")
            null
        }
    }
}
