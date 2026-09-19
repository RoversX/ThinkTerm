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
class TerminalModel(
    private val context: Context,
    private val store: HostStore,
    val host: Host,
) {
    val core: Core

    // What the screen shows, as Compose state (written on the main thread).
    var tabs by mutableStateOf<TabsView?>(null)
        private set
    var status by mutableStateOf<StatusView?>(null)
        private set
    var title by mutableStateOf("")
        private set
    var connection by mutableStateOf("")
        private set
    var background by mutableStateOf(Color(0xFF1C1C1C))
        private set
    var ctrlSticky by mutableStateOf(false)
    var altSticky by mutableStateOf(false)

    private val frameNeeded = AtomicBoolean(false)
    private val changePending = AtomicBoolean(false)
    private val main = Handler(Looper.getMainLooper())
    private val choreographer = Choreographer.getInstance()
    private var running = true
    private var connected = false

    /// Set by the view once the surface is attached; the connection waits
    /// for it so the first frame has somewhere to go.
    var generation: ULong = 0uL

    private val tick = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            if (!running) return
            if (frameNeeded.getAndSet(false)) core.render()
            if (changePending.getAndSet(false)) refreshViews()
            choreographer.postFrameCallback(this)
        }
    }

    init {
        core = Core(Sink())
        choreographer.postFrameCallback(tick)
    }

    // MARK: connection

    /// Connect once; the surface must be attached first so the core has a
    /// size to lay the panes out in.
    fun connect() {
        if (connected) return
        connected = true
        val secret = if (host.id == Host.PROBE_ID) probeKey() else store.secret(host.id)
        val passphrase = if (host.id == Host.PROBE_ID) null else store.passphrase(host.id)
        Log.i("thinkterm", "shell: connecting ${host.address}, ${secret.length} chars of secret")
        core.connect(
            host = host.hostname,
            port = host.port.toUShort(),
            user = host.user,
            authKind = host.auth,
            secret = secret,
            passphrase = passphrase,
            knownHost = host.knownHost,
            remoteCommand = host.remoteCommand,
            deviceId = store.deviceId,
            keepaliveSecs = 15u,
            fontPaths = Assets.fontPaths(context),
            sizePt = 11.0,
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

    /// Leaving the screen for good: the frame loop stops, then the core.
    fun shutdown() {
        running = false
        choreographer.removeFrameCallback(tick)
        core.disconnect()
        core.shutdown()
    }

    // MARK: input

    fun key(name: String, ctrl: Boolean = false, alt: Boolean = false, shift: Boolean = false) {
        core.key(name, ctrl || takeCtrl(), alt || takeAlt(), shift)
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

    private fun takeCtrl(): Boolean = ctrlSticky.also { if (it) ctrlSticky = false }
    private fun takeAlt(): Boolean = altSticky.also { if (it) altSticky = false }

    fun chromeClick(action: String, pane: Int? = null, tab: Int? = null) {
        core.chromeClick(action, pane?.toUInt(), tab?.toUInt())
    }

    // MARK: the views

    /// Pull every view the screen shows. Called on the main thread after
    /// the core said something changed; a burst of changes is one pull.
    private fun refreshViews() {
        val tabs = Views.tabs(core.view("tabs"))
        if (tabs != this.tabs) this.tabs = tabs
        val status = Views.status(core.view("status"))
        if (status != this.status) this.status = status
    }

    // MARK: what the core tells the shell

    private inner class Sink : Notify {
        override fun onFrameNeeded() {
            frameNeeded.set(true)
        }

        override fun onStatus(status: String) {
            main.post { connection = status }
        }

        override fun onLog(line: String) {
            Log.i("thinkterm", line)
        }

        override fun onTitle(title: String) {
            main.post { this@TerminalModel.title = title }
        }

        override fun onChange() {
            changePending.set(true)
        }

        override fun onClipboard(text: String) {
            main.post {
                val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager
                cm?.setPrimaryClip(ClipData.newPlainText("thinkterm", text))
            }
        }

        override fun onFocusInput() {
            main.post { onFocusRequested?.invoke() }
        }

        override fun onImeAnchor(left: Double, top: Double, width: Double, height: Double) {
            // Nothing yet: the Android IME is not asked where the caret is.
        }

        override fun onHostKey(fingerprint: String) {
            main.post { store.rememberHostKey(host.id, fingerprint) }
        }

        override fun onPublished(key: String, value: String) {
            if (key != "bg") return
            val color = parseHex(value) ?: return
            main.post { if (background != color) background = color }
        }

        override fun onBell() {
            main.post { buzz() }
        }

        override fun onPreview(pane: UInt, rows: String) {
            // The overview is not built yet.
        }
    }

    /// The view sets this so the core can ask for the keyboard.
    var onFocusRequested: (() -> Unit)? = null

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

    private fun parseHex(value: String): Color? {
        val hex = value.removePrefix("#")
        if (hex.length != 6 && hex.length != 8) return null
        return try {
            val n = hex.toLong(16)
            if (hex.length == 6) Color(0xFF000000L.or(n).toInt()) else Color(n.toInt())
        } catch (e: Throwable) {
            null
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
            if (out.exists() && out.length() > 0) continue
            try {
                context.assets.open(name).use { input -> out.outputStream().use { input.copyTo(it) } }
            } catch (e: Throwable) {
                Log.w("thinkterm", "font $name not installed: $e")
            }
        }
    }

    /// The base face first, the symbols fallback after it.
    fun fontPaths(context: Context): List<String> =
        listOf("JetBrainsMono-Regular.ttf", "SymbolsNerdFontMono-Regular.ttf")
            .map { java.io.File(context.filesDir, it).absolutePath }
            .filter { java.io.File(it).exists() }
}
