package com.roversx.thinkterm

import android.util.Log
import org.json.JSONArray
import org.json.JSONObject

// The App's views, as its JSON names them (thinkterm-web/src/views.rs,
// chrome.rs, navbar.rs and app.rs are the reference; ios/Sources/Views.swift
// is the same set in Swift). org.json is in the platform, so no codegen.

data class PaneView(val pane: Int, val title: String, val current: Boolean)

data class TabEntry(
    val tab: Int,
    val window: Int,
    val title: String,
    val label: String,
    val target: Int,
    val current: Boolean,
    val panes: List<PaneView>,
)

data class Controls(val following: Boolean, val fit: Boolean, val closing: Int?)

data class TabsView(val tabs: List<TabEntry>, val controls: Controls) {
    val current: TabEntry? get() = tabs.firstOrNull { it.current }
}

data class NavRect(val pane: Int, val left: Double, val top: Double, val width: Double, val height: Double)

data class CapsuleView(val pane: Int, val title: String, val busy: Boolean, val current: Boolean)

data class NavView(
    val rect: NavRect,
    val members: List<CapsuleView>,
    val focused: Boolean,
    val zoomed: Boolean,
    val closing: Boolean,
)

/// The desktop's sidebar layer with its tabs: `App::threads_view`.
data class ThreadsView(val space: String, val threads: List<ThreadView>) {
    val current: ThreadView? get() = threads.firstOrNull { it.current }
}

data class ThreadView(
    val id: String,
    val name: String,
    val project: String,
    val projectId: String,
    val status: String,
    val dot: String,
    val live: Boolean,
    val pinned: Boolean,
    val unread: Boolean,
    val current: Boolean,
    val window: Int?,
    val tabs: List<ThreadTab>,
)

data class ThreadTab(
    val tab: Int,
    val title: String,
    val target: Int,
    val current: Boolean,
    val panes: List<PaneRef>,
)

data class PaneRef(val pane: Int, val title: String)

/// The tree whole, every Space with its projects and threads: `App::tree_view`.
data class TreeView(val spaces: List<TreeSpace>)

data class TreeSpace(val id: String, val name: String, val current: Boolean, val projects: List<TreeProject>)

data class TreeProject(val id: String, val name: String, val path: String, val threads: List<ThreadRow>)

/// A thumbnail row: `App::PreviewRow`.
data class PreviewRow(val runs: List<PreviewRun>)

data class PreviewRun(val text: String, val fg: String)

data class Toast(val text: String, val sticky: Boolean, val at: Double)

data class Card(val title: String, val hint: String, val state: String, val action: String)

data class StatusView(val toast: Toast?, val card: Card?, val summary: String)

/// One row of the sidebar, tagged by `kind` in the JSON.
sealed class SideRow {
    abstract val key: String

    data class Space(val id: String, val name: String) : SideRow() {
        override val key get() = "space:$id"
    }

    object NewThread : SideRow() {
        override val key get() = "new-thread"
    }

    object Pinned : SideRow() {
        override val key get() = "pinned"
    }

    object Workspaces : SideRow() {
        override val key get() = "workspaces"
    }

    data class Project(val id: String, val name: String, val path: String, val collapsed: Boolean, val archived: Boolean) : SideRow() {
        override val key get() = "project:$id"
    }

    data class Thread(val row: ThreadRow) : SideRow() {
        override val key get() = "thread:${row.id}"
    }

    data class Archived(val count: Int, val open: Boolean, val label: String) : SideRow() {
        override val key get() = "archived"
    }

    object Others : SideRow() {
        override val key get() = "others"
    }

    data class Window(val id: Int, val title: String, val selected: Boolean) : SideRow() {
        override val key get() = "window:$id"
    }
}

data class ThreadRow(
    val id: String,
    val project: String,
    val name: String,
    val status: String,
    val dot: String,
    val pinned: Boolean,
    val unread: Boolean,
    val live: Boolean,
    val selected: Boolean,
    val deleting: Boolean,
)

data class Editing(val kind: String, val id: String?)

data class SidebarView(
    val rows: List<SideRow>,
    val editing: Editing,
    val space: String?,
    val newProjectError: String?,
)

data class MenuItem(
    val id: String,
    val label: String,
    val icon: String?,
    val kind: String,
    val enabled: Boolean,
    val checked: Boolean,
    val submenu: List<MenuItem>,
) {
    /// Separators and headers share an empty id; rows need a stable one.
    val rowId: String get() = if (id.isEmpty()) "$kind:$label" else id
}

data class MenuOutcome(val handled: Boolean, val copy: String?, val paste: Boolean)

/// The focused pane's text, for a selection: `App::screen_text`.
data class ScreenText(
    val cols: Int,
    val rows: Int,
    val text: String,
    val originX: Double,
    val originY: Double,
    val cellW: Double,
    val cellH: Double,
    /// (row, col) of both ends, ordered, inclusive; null for none.
    val selection: Pair<Pair<Int, Int>, Pair<Int, Int>>?,
)

/// What the layout view carries that the shell reads.
data class LayoutView(
    val cellW: Double,
    val cellH: Double,
    val fontPt: Double,
    /// Rows above the bottom (with the fraction a smooth scroll is part
    /// way through), and rows there are to scroll through.
    val scrollAbove: Double,
    val scrollMax: Int,
    val focused: Int,
)

object Views {
    fun tabs(json: String): TabsView? {
        val root = obj(json) ?: return null
        val tabs = root.optJSONArray("tabs").map { t ->
            TabEntry(
                tab = t.optInt("tab"),
                window = t.optInt("window"),
                title = t.optString("title"),
                label = t.optString("label"),
                target = t.optInt("target"),
                current = t.optBoolean("current"),
                panes = t.optJSONArray("panes").map { p ->
                    PaneView(p.optInt("pane"), p.optString("title"), p.optBoolean("current"))
                },
            )
        }
        val c = root.optJSONObject("controls")
        val controls = Controls(
            following = c?.optBoolean("following") ?: false,
            fit = c?.optBoolean("fit") ?: false,
            closing = c?.takeIf { it.has("closing") && !it.isNull("closing") }?.optInt("closing"),
        )
        return TabsView(tabs, controls)
    }

    fun navs(json: String): List<NavView> {
        val arr = array(json) ?: return emptyList()
        return arr.map { n ->
            val r = n.optJSONObject("rect") ?: JSONObject()
            NavView(
                rect = NavRect(r.optInt("pane"), r.optDouble("left"), r.optDouble("top"), r.optDouble("width"), r.optDouble("height")),
                members = n.optJSONArray("members").map { m ->
                    CapsuleView(m.optInt("pane"), m.optString("title"), m.optBoolean("busy"), m.optBoolean("current"))
                },
                focused = n.optBoolean("focused"),
                zoomed = n.optBoolean("zoomed"),
                closing = n.optBoolean("closing"),
            )
        }
    }

    fun threads(json: String): ThreadsView? {
        val root = obj(json) ?: return null
        return ThreadsView(
            space = root.optString("space"),
            threads = root.optJSONArray("threads").map { t ->
                ThreadView(
                    id = t.optString("id"),
                    name = t.optString("name"),
                    project = t.optString("project"),
                    projectId = t.optString("project_id"),
                    status = t.optString("status"),
                    dot = t.optString("dot"),
                    live = t.optBoolean("live"),
                    pinned = t.optBoolean("pinned"),
                    unread = t.optBoolean("unread"),
                    current = t.optBoolean("current"),
                    window = t.takeIf { it.has("window") && !it.isNull("window") }?.optInt("window"),
                    tabs = t.optJSONArray("tabs").map { tab ->
                        ThreadTab(
                            tab = tab.optInt("tab"),
                            title = tab.optString("title"),
                            target = tab.optInt("target"),
                            current = tab.optBoolean("current"),
                            panes = tab.optJSONArray("panes").map { p -> PaneRef(p.optInt("pane"), p.optString("title")) },
                        )
                    },
                )
            },
        )
    }

    fun tree(json: String): TreeView? {
        val root = obj(json) ?: return null
        return TreeView(
            root.optJSONArray("spaces").map { sp ->
                TreeSpace(
                    id = sp.optString("id"),
                    name = sp.optString("name"),
                    current = sp.optBoolean("current"),
                    projects = sp.optJSONArray("projects").map { pr ->
                        TreeProject(
                            id = pr.optString("id"),
                            name = pr.optString("name"),
                            path = pr.optString("path"),
                            threads = pr.optJSONArray("threads").map { threadRow(it) },
                        )
                    },
                )
            }
        )
    }

    private fun threadRow(r: JSONObject) = ThreadRow(
        id = r.optString("id"),
        project = r.optString("project"),
        name = r.optString("name"),
        status = r.optString("status"),
        dot = r.optString("dot"),
        pinned = r.optBoolean("pinned"),
        unread = r.optBoolean("unread"),
        live = r.optBoolean("live"),
        selected = r.optBoolean("selected"),
        deleting = r.optBoolean("deleting"),
    )

    fun previewRows(json: String): List<PreviewRow> {
        val arr = array(json) ?: return emptyList()
        return arr.map { row ->
            PreviewRow(row.optJSONArray("runs").map { run -> PreviewRun(run.optString("text"), run.optString("fg")) })
        }
    }

    fun status(json: String): StatusView? {
        val root = obj(json) ?: return null
        val toast = root.optJSONObject("toast")?.let {
            Toast(it.optString("text"), it.optBoolean("sticky"), it.optDouble("at"))
        }
        val card = root.optJSONObject("card")?.let {
            Card(it.optString("title"), it.optString("hint"), it.optString("state"), it.optString("action"))
        }
        return StatusView(toast, card, root.optString("summary"))
    }

    fun sidebar(json: String): SidebarView? {
        val root = obj(json) ?: return null
        val rows = root.optJSONArray("rows").mapNotNull { r ->
            when (r.optString("kind")) {
                "space" -> SideRow.Space(r.optString("id"), r.optString("name"))
                "new-thread" -> SideRow.NewThread
                "pinned" -> SideRow.Pinned
                "workspaces" -> SideRow.Workspaces
                "project" -> SideRow.Project(
                    r.optString("id"), r.optString("name"), r.optString("path"),
                    r.optBoolean("collapsed"), r.optBoolean("archived"),
                )
                "thread" -> SideRow.Thread(
                    ThreadRow(
                        id = r.optString("id"),
                        project = r.optString("project"),
                        name = r.optString("name"),
                        status = r.optString("status"),
                        dot = r.optString("dot"),
                        pinned = r.optBoolean("pinned"),
                        unread = r.optBoolean("unread"),
                        live = r.optBoolean("live"),
                        selected = r.optBoolean("selected"),
                        deleting = r.optBoolean("deleting"),
                    )
                )
                "archived" -> SideRow.Archived(r.optInt("count"), r.optBoolean("open"), r.optString("label"))
                "others" -> SideRow.Others
                "window" -> SideRow.Window(r.optInt("id"), r.optString("title"), r.optBoolean("selected"))
                else -> null
            }
        }
        val e = root.optJSONObject("editing")
        return SidebarView(
            rows = rows,
            editing = Editing(e?.optString("kind") ?: "none", e?.takeIf { it.has("id") && !it.isNull("id") }?.optString("id")),
            space = root.takeIf { it.has("space") && !it.isNull("space") }?.optString("space"),
            newProjectError = root.takeIf { it.has("new_project_error") && !it.isNull("new_project_error") }?.optString("new_project_error"),
        )
    }

    fun menu(json: String): List<MenuItem> = array(json)?.map { menuItem(it) } ?: emptyList()

    private fun menuItem(o: JSONObject): MenuItem = MenuItem(
        id = o.optString("id"),
        label = o.optString("label"),
        icon = o.takeIf { it.has("icon") && !it.isNull("icon") }?.optString("icon"),
        kind = o.optString("kind", "item"),
        enabled = o.optBoolean("enabled", true),
        checked = o.optBoolean("checked"),
        submenu = o.optJSONArray("submenu").map { menuItem(it) },
    )

    fun menuOutcome(json: String): MenuOutcome? {
        val root = obj(json) ?: return null
        return MenuOutcome(
            handled = root.optBoolean("handled"),
            copy = root.takeIf { it.has("copy") && !it.isNull("copy") }?.optString("copy"),
            paste = root.optBoolean("paste"),
        )
    }

    fun screenText(json: String): ScreenText? {
        val root = obj(json) ?: return null
        val origin = root.optJSONArray("origin")
        val cell = root.optJSONArray("cell")
        val sel = root.optJSONArray("selection")?.let { s ->
            val a = s.optJSONArray(0) ?: return@let null
            val b = s.optJSONArray(1) ?: return@let null
            Pair(Pair(a.optInt(0), a.optInt(1)), Pair(b.optInt(0), b.optInt(1)))
        }
        return ScreenText(
            cols = root.optInt("cols"),
            rows = root.optInt("rows"),
            text = root.optString("text"),
            originX = origin?.optDouble(0) ?: 0.0,
            originY = origin?.optDouble(1) ?: 0.0,
            cellW = cell?.optDouble(0) ?: 1.0,
            cellH = cell?.optDouble(1) ?: 1.0,
            selection = sel,
        )
    }

    fun layout(json: String): LayoutView? {
        val root = obj(json) ?: return null
        val cell = root.optJSONArray("cell")
        val scroll = root.optJSONArray("scroll")
        return LayoutView(
            cellW = cell?.optDouble(0) ?: 0.0,
            cellH = cell?.optDouble(1) ?: 0.0,
            fontPt = root.optDouble("font_pt"),
            scrollAbove = scroll?.optDouble(0) ?: 0.0,
            scrollMax = scroll?.optInt(1) ?: 0,
            focused = root.optInt("focused"),
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

    private fun array(json: String): JSONArray? {
        if (json.isEmpty() || json == "null") return null
        return try {
            JSONArray(json)
        } catch (e: Throwable) {
            Log.w("thinkterm", "view decode failed: ${json.take(200)}")
            null
        }
    }

    private inline fun <T> JSONArray?.map(f: (JSONObject) -> T): List<T> {
        if (this == null) return emptyList()
        val out = ArrayList<T>(length())
        for (i in 0 until length()) {
            val o = optJSONObject(i) ?: continue
            out.add(f(o))
        }
        return out
    }

    private inline fun <T> JSONArray?.mapNotNull(f: (JSONObject) -> T?): List<T> {
        if (this == null) return emptyList()
        val out = ArrayList<T>(length())
        for (i in 0 until length()) {
            val o = optJSONObject(i) ?: continue
            f(o)?.let { out.add(it) }
        }
        return out
    }
}
