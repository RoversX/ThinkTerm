package com.roversx.thinkterm

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.util.Log
import android.view.Choreographer
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.Color
import com.roversx.thinkterm.core.Core
import com.roversx.thinkterm.core.Notify
import java.util.concurrent.atomic.AtomicBoolean

/// The shell's side of the thread contract. `Notify` calls arrive on the
/// core thread; nothing here blocks them and nothing here calls back into
/// the core from inside one. The Choreographer callback is the only place
/// `render` is called; the core only ever *asks* for a frame.
///
/// One model per connection: the screen that shows a host makes one and
/// shuts it down when it goes. The shape follows ios/Sources/TerminalModel.swift.
class TerminalModel(
    private val context: Context,
    val store: HostStore,
    host: Host,
) {
    val core: Core
    val settings = AppSettings.get(context)

    /// The host this screen shows; edited in place from the failure card.
    var host by mutableStateOf(host)

    // The App's views, refreshed when the core says something changed.
    var tabs by mutableStateOf<TabsView?>(null)
        private set
    var threads by mutableStateOf<ThreadsView?>(null)
        private set
    var sidebar by mutableStateOf<SidebarView?>(null)
        private set
    var tree by mutableStateOf<TreeView?>(null)
        private set
    var navs by mutableStateOf<List<NavView>>(emptyList())
        private set
    var status by mutableStateOf<StatusView?>(null)
        private set
    /// The last rows of panes the overview asked about, by pane.
    val previews = mutableStateMapOf<Int, List<PreviewRow>>()
    var title by mutableStateOf("")
        private set
    var connection by mutableStateOf("")
        private set
    var attached by mutableStateOf(false)
        private set
    var composing by mutableStateOf<String?>(null)
    var ctrlSticky by mutableStateOf(false)
    var altSticky by mutableStateOf(false)
    var logText by mutableStateOf("")
        private set
    var stats by mutableStateOf("")
        private set
    /// A selection was just copied; the status line says so for a moment.
    var copied by mutableStateOf(false)
        private set
    /// The App's remark for the status line: a passing one goes after a
    /// few seconds, a sticky one (the connection is down) stays until the
    /// App takes it back.
    var toastText by mutableStateOf<String?>(null)
        private set
    /// The App says its connection is down and it is redialing.
    var reconnecting by mutableStateOf(false)
        private set
    /// The terminal's background, as the App paints it: the chrome around
    /// the terminal takes the same colour.
    var background by mutableStateOf(Color(0xFF1C1C1C))
        private set
    /// The App's font size in points, from its layout view.
    var fontPt by mutableStateOf(11.0)
        private set
    /// The focused pane's place in its scrollback: rows above the bottom,
    /// and rows there are; changes for a moment show the scrollbar.
    var scrollAbove by mutableStateOf(0)
        private set
    var scrollMax by mutableStateOf(0)
        private set
    var scrollShown by mutableStateOf(false)
        private set
    /// The App's cell size in dp, from its layout view.
    var cellWidth = 8.0
        private set
    var cellHeight = 20.0
        private set
    /// The log view is open: the stats are worth refreshing.
    var wantsStats = false
    /// Where the caret is, in dp from the terminal's origin (the App's
    /// IME anchor), for the selection handles and the composition popup.
    var cursorLeft = 8.0
    var cursorTop = 8.0
    var cursorWidth = 2.0
    var cursorHeight = 20.0

    /// Set by the view once the surface is attached; the connection waits
    /// for it so the first frame has somewhere to go.
    var generation: ULong = 0uL

    /// The view sets these so the core can ask for the keyboard and the
    /// selection can be redrawn when the screen changes.
    var onFocusRequested: (() -> Unit)? = null
    /// The connection changed under an open IME composition: the view
    /// lets it go, so it does not land in whatever comes next.
    var onDropComposition: (() -> Unit)? = null
    /// The keyboard's options changed: the view starts its input over so
    /// the IME reads them again.
    var onImeOptionsChanged: (() -> Unit)? = null
    private var appliedIncognito = settings.incognitoKeyboard
    var onScreenChanged: (() -> Unit)? = null
    var onHideKeyboard: (() -> Unit)? = null
    /// A selection stands: where its copy chip goes, in dp from the
    /// terminal's origin (the head cell's top-left), or null for none.
    var selectionMenuAt by mutableStateOf<Pair<Double, Double>?>(null)

    val smoothScroll: Boolean get() = settings.smoothScroll

    private val frameNeeded = AtomicBoolean(false)
    private val changePending = AtomicBoolean(false)
    private val main = Handler(Looper.getMainLooper())
    private val choreographer = Choreographer.getInstance()
    private var running = true
    private var connected = false
    private val logLines = ArrayDeque<String>()
    private var toastShown: String? = null
    private var appliedFamily = ""

    private val tick = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (!running) return
            if (frameNeeded.getAndSet(false)) core.render()
            if (changePending.getAndSet(false)) refreshViews()
            choreographer.postFrameCallback(this)
        }
    }

    private val statsTick = object : Runnable {
        override fun run() {
            if (!running) return
            if (wantsStats || settings.devMode) {
                val s = core.stats()
                if (s != stats) stats = s
            }
            main.postDelayed(this, 500)
        }
    }

    init {
        core = Core(Sink())
        choreographer.postFrameCallback(tick)
        main.postDelayed(statsTick, 500)
        applyScheme()
        applyScrollMode()
        applyTerminalPrefs()
        appliedFamily = settings.fontFamily
    }

    /// The preferences the open terminal follows: the screen calls this
    /// when any of them changes (a snapshot read in a composable).
    fun settingsChanged() {
        applyScheme()
        applyScrollMode()
        applyTerminalPrefs()
        if (settings.incognitoKeyboard != appliedIncognito) {
            appliedIncognito = settings.incognitoKeyboard
            onImeOptionsChanged?.invoke()
        }
        // A face is shaped at connect time: the connection is made again.
        if (settings.fontFamily != appliedFamily) {
            appliedFamily = settings.fontFamily
            if (connection.startsWith("pane ")) {
                disconnect()
                connect()
            }
        }
    }

    // MARK: connection

    /// The chosen face first, the symbols fallback after it.
    private fun fontPaths(): List<String> {
        val face = if (settings.fontFamily == "Fira Code") "FiraCode-Regular.ttf" else "JetBrainsMono-Regular.ttf"
        return Assets.fontPaths(context, face)
    }

    /// Connect to the model's host with the secret from the store.
    fun connect() {
        connected = true
        val h = host
        val secret = if (h.id == Host.PROBE_ID) probeKey() else store.secret(h.id)
        val passphrase = if (h.id == Host.PROBE_ID) null else store.passphrase(h.id)
        log("shell: connecting ${h.address}, ${secret.length} chars of secret")
        core.connect(
            host = h.hostname,
            port = h.port.coerceIn(1, 65535).toUShort(),
            user = h.user,
            authKind = h.auth,
            secret = secret,
            passphrase = passphrase,
            knownHost = h.knownHost,
            remoteCommand = h.remoteCommand,
            deviceId = store.deviceId,
            keepaliveSecs = settings.keepAliveSeconds.coerceAtLeast(0).toUInt(),
            fontPaths = fontPaths(),
            sizePt = settings.fontSize,
            painter = AndroidGlyphPainter(),
        )
    }

    /// The probe's throwaway key travels in the APK, not the preferences.
    private fun probeKey(): String = try {
        context.assets.open("probe_key").bufferedReader().use { it.readText() }
    } catch (e: Throwable) {
        Log.w("thinkterm", "no probe_key asset: $e")
        ""
    }

    fun disconnect() {
        connected = false
        core.disconnect()
    }

    val isConnected: Boolean
        get() = connection.isNotEmpty() && !connection.startsWith("disconnected") && !connection.startsWith("failed")

    /// Leaving the screen for good: the frame loop stops, then the core.
    fun shutdown() {
        running = false
        choreographer.removeFrameCallback(tick)
        main.removeCallbacks(statsTick)
        core.disconnect()
        core.shutdown()
    }

    /// The app left the screen. The socket lives as long as the process
    /// does; with the setting off, it is dropped now and redialed on return.
    fun enteredBackground() {
        if (!settings.keepSessionInBackground && connection.startsWith("pane ")) {
            disconnect()
        }
    }

    fun enteredForeground() {
        if (!settings.keepSessionInBackground && connection.startsWith("disconnected")) {
            connect()
        }
    }

    // MARK: input

    fun key(name: String, ctrl: Boolean = false, alt: Boolean = false, shift: Boolean = false) {
        // A sticky modifier is spent by the next key whether or not that
        // key brought the modifier itself, so it never lingers past it.
        val stickyCtrl = takeCtrl()
        val stickyAlt = takeAlt()
        core.key(name, ctrl || stickyCtrl, alt || stickyAlt, shift)
    }

    /// Text from the keyboard. A sticky Ctrl or Alt turns a single
    /// character into a chord instead.
    fun text(text: String) {
        if ((ctrlSticky || altSticky) && text.length == 1) {
            key(text)
            return
        }
        core.text(text)
    }

    fun pasteFromClipboard() {
        val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager ?: return
        val text = cm.primaryClip?.getItemAt(0)?.coerceToText(context)?.toString() ?: return
        if (text.isNotEmpty()) core.paste(text)
    }

    fun copyToClipboard(text: String) {
        val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager ?: return
        cm.setPrimaryClip(ClipData.newPlainText("thinkterm", text))
        showCopied()
    }

    private fun takeCtrl(): Boolean = ctrlSticky.also { if (it) ctrlSticky = false }
    private fun takeAlt(): Boolean = altSticky.also { if (it) altSticky = false }

    fun pointer(kind: String, x: Double, y: Double) = core.pointer(kind, x, y)

    fun wheel(x: Double, y: Double, lines: Double) = core.wheel(x, y, lines)

    fun wheelPx(x: Double, y: Double, px: Double) = core.wheelPx(x, y, px)

    fun stepFont(by: Double) = core.stepFont(by)

    /// The tab `by` places along the strip from the current one, shown.
    fun switchTab(by: Int) {
        val list = tabs?.tabs ?: return
        val at = list.indexOfFirst { it.current }
        if (at < 0) return
        val next = at + by
        if (next !in list.indices) return
        chromeClick("pane", pane = list[next].target)
    }

    /// The last rows of a pane, for a thumbnail; `previews` fills in.
    fun requestPreview(pane: Int, rows: Int = 8) = core.requestPreview(pane.toUInt(), rows.toUInt())

    // MARK: the App's chrome

    fun chromeClick(action: String, pane: Int? = null, tab: Int? = null) {
        core.chromeClick(action, pane?.toUInt(), tab?.toUInt())
    }

    fun sideClick(kind: String, id: String? = null, flag: Boolean? = null) = core.sideClick(kind, id, flag)

    fun sideKey(key: String, value: String) = core.sideKey(key, value)

    /// Show a thread, switching the Space on show to its own first when
    /// it is in another one, so the strips follow.
    fun openThread(id: String, spaceId: String?) {
        if (spaceId != null && tree?.spaces?.firstOrNull { it.current }?.id != spaceId) core.setSpace(spaceId)
        sideClick("thread", id)
    }

    fun setSpace(id: String) = core.setSpace(id)

    fun contextMenu(kind: String, id: String): List<MenuItem> = Views.menu(core.contextMenu(kind, id))

    /// Run a menu row. Copy and paste are the shell's to do.
    fun menuAction(id: String) {
        val outcome = Views.menuOutcome(core.menuAction(id)) ?: return
        outcome.copy?.let { copyToClipboard(it) }
        if (outcome.paste) pasteFromClipboard()
    }

    fun takeOver() = core.takeOver()

    /// The selected text to the clipboard, and the selection put away.
    fun copySelection() {
        core.selectedText()?.takeIf { it.isNotEmpty() }?.let { copyToClipboard(it) }
        clearSelection()
    }

    fun clearSelection() {
        core.clearSelection()
        selectionMenuAt = null
    }

    fun showCopied() {
        copied = true
        main.postDelayed({ copied = false }, 1200)
    }

    // MARK: the views

    /// Pull every view the screen shows. Called on the main thread after
    /// the core said something changed; a burst of changes is one pull.
    /// Assigned only on a change: a state write redraws the screen, and
    /// the core reports changes as often as output comes.
    fun refreshViews() {
        onScreenChanged?.invoke()
        val tabs = Views.tabs(core.view("tabs"))
        if (tabs != this.tabs) this.tabs = tabs
        val sidebar = Views.sidebar(core.view("sidebar"))
        if (sidebar != this.sidebar) this.sidebar = sidebar
        val threads = Views.threads(core.view("threads"))
        if (threads != this.threads) this.threads = threads
        val tree = Views.tree(core.view("tree"))
        if (tree != this.tree) this.tree = tree
        val navs = Views.navs(core.view("navs"))
        if (navs != this.navs) this.navs = navs
        val status = Views.status(core.view("status"))
        if (status != this.status) {
            this.status = status
            showToast(status?.toast)
        }
        Views.layout(core.view("layout"))?.let { layout ->
            if (layout.cellH > 0) cellHeight = layout.cellH
            if (layout.cellW > 0) cellWidth = layout.cellW
            if (layout.fontPt > 0 && layout.fontPt != fontPt) fontPt = layout.fontPt
            if (layout.scrollAbove != scrollAbove || layout.scrollMax != scrollMax) {
                scrollAbove = layout.scrollAbove
                scrollMax = layout.scrollMax
                scrollShown = true
                main.removeCallbacks(hideScrollbar)
                main.postDelayed(hideScrollbar, 1000)
            }
        }
    }

    private val hideScrollbar = Runnable { scrollShown = false }

    private fun showToast(toast: Toast?) {
        main.removeCallbacks(expireToast)
        val sticky = toast?.sticky ?: false
        if (reconnecting != sticky) reconnecting = sticky
        if (toast == null || toast.text.isEmpty()) {
            if (toastText != null) toastText = null
            return
        }
        toastText = toast.text
        toastShown = toast.text
        if (!sticky) main.postDelayed(expireToast, 4000)
    }

    private val expireToast = Runnable { if (toastText == toastShown) toastText = null }

    // MARK: the preferences the App honours

    private fun applyScheme() {
        val name = settings.schemeName
        val json = Schemes.json(name)
        if (name == Schemes.FOLLOW_DESKTOP || json == null) {
            core.setSetting("terminal-scheme", "\"desktop\"")
            core.setPalette(null)
        } else {
            core.setSetting("terminal-scheme", "\"" + name.replace("\"", "\\\"") + "\"")
            core.setPalette(json)
        }
    }

    private fun applyScrollMode() {
        core.setSetting("scroll-mode", if (settings.smoothScroll) "\"smooth\"" else "\"stepped\"")
    }

    private fun applyTerminalPrefs() {
        val style = settings.cursorStyle.takeIf { it in listOf("auto", "block", "bar", "underline") } ?: "auto"
        core.setSetting("cursor-style", "\"$style\"")
        core.setSetting("cursor-blink", settings.cursorBlink.toString())
        val contrast = when (settings.contrast) { "3" -> 3.0; "45" -> 4.5; "7" -> 7.0; else -> 0.0 }
        core.setSetting("min-contrast", contrast.toString())
        core.setSetting("resize-mode", if (settings.resizeMode == "release") "\"release\"" else "\"live\"")
        core.setSetting("auto-reconnect", settings.autoReconnect.toString())
        core.setSetting("pane-bars", settings.paneBars.toString())
    }

    // MARK: the log

    fun log(line: String) {
        Log.i("thinkterm", line)
        logLines.addLast(line)
        while (logLines.size > 60) logLines.removeFirst()
        logText = logLines.joinToString("\n")
    }

    // MARK: what the core tells the shell (core thread)

    private inner class Sink : Notify {
        override fun onFrameNeeded() {
            frameNeeded.set(true)
        }

        override fun onStatus(status: String) {
            main.post {
                connection = status
                log("status: $status")
                if (status.startsWith("pane ")) {
                    attached = true
                    if (host.id != Host.PROBE_ID) store.touchConnected(host.id)
                }
                if (status.startsWith("connecting") || status.contains("reconnect") || status.startsWith("disconnected")) {
                    composing = null
                    onDropComposition?.invoke()
                }
                changePending.set(true)
            }
        }

        override fun onLog(line: String) {
            main.post { log("core: $line") }
        }

        override fun onTitle(title: String) {
            main.post { if (this@TerminalModel.title != title) this@TerminalModel.title = title }
        }

        override fun onChange() {
            changePending.set(true)
        }

        override fun onClipboard(text: String) {
            main.post { copyToClipboard(text) }
        }

        override fun onFocusInput() {
            main.post { onFocusRequested?.invoke() }
        }

        override fun onImeAnchor(left: Double, top: Double, width: Double, height: Double) {
            main.post {
                cursorLeft = left
                cursorTop = top
                cursorWidth = width
                cursorHeight = height
            }
        }

        override fun onHostKey(fingerprint: String) {
            main.post {
                log("host key $fingerprint")
                if (host.id != Host.PROBE_ID) store.rememberHostKey(host.id, fingerprint)
            }
        }

        override fun onPublished(key: String, value: String) {
            if (key != "bg") return
            val color = parseHex(value) ?: return
            main.post { if (background != color) background = color }
        }

        override fun onBell() {
            main.post { if (settings.bell) buzz() }
        }

        override fun onPreview(pane: UInt, rows: String) {
            val decoded = Views.previewRows(rows)
            main.post { previews[pane.toInt()] = decoded }
        }
    }

    private fun buzz() {
        try {
            val v = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                (context.getSystemService(Context.VIBRATOR_MANAGER_SERVICE) as VibratorManager).defaultVibrator
            } else {
                @Suppress("DEPRECATION")
                context.getSystemService(Context.VIBRATOR_SERVICE) as Vibrator
            }
            v.vibrate(VibrationEffect.createOneShot(20, VibrationEffect.DEFAULT_AMPLITUDE))
        } catch (e: Throwable) {
            // No vibrator, or no permission: the bell is silent.
        }
    }
}

/// The fonts the core shapes with: copied out of the APK once, because
/// the core opens them as files.
object Assets {
    private val faces = listOf("JetBrainsMono-Regular.ttf", "SymbolsNerdFontMono-Regular.ttf", "FiraCode-Regular.ttf")

    fun install(context: Context) {
        for (name in faces) {
            val out = java.io.File(context.filesDir, name)
            // The APK's copy is the truth: a file of another size is a
            // copy that was cut short (or a font that changed) and is
            // made again. An unknown size settles for a file that exists.
            val wanted = try {
                context.assets.openFd(name).use { it.length }
            } catch (e: Throwable) {
                -1L
            }
            if (out.exists() && out.length() > 0 && (wanted < 0 || out.length() == wanted)) continue
            // Written beside its name and renamed into place, so a copy
            // interrupted half-way never passes for a font.
            val part = java.io.File(context.filesDir, "$name.part")
            try {
                context.assets.open(name).use { input -> part.outputStream().use { input.copyTo(it) } }
                if (wanted >= 0 && part.length() != wanted) throw java.io.IOException("short copy")
                if (!part.renameTo(out)) throw java.io.IOException("rename failed")
            } catch (e: Throwable) {
                part.delete()
                Log.w("thinkterm", "font $name not installed: $e")
            }
        }
    }

    /// The chosen face first, the symbols fallback after it.
    fun fontPaths(context: Context, face: String = "JetBrainsMono-Regular.ttf"): List<String> =
        listOf(face, "SymbolsNerdFontMono-Regular.ttf")
            .map { java.io.File(context.filesDir, it).absolutePath }
            .filter { java.io.File(it).exists() }
}
