package com.roversx.thinkterm

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.res.Configuration
import android.graphics.PixelFormat
import android.hardware.HardwareBuffer
import android.media.ImageReader
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.os.VibrationEffect
import android.os.Vibrator
import android.os.VibratorManager
import android.util.Log
import android.view.Choreographer
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.Color
import com.roversx.thinkterm.core.ChatAttachment
import com.roversx.thinkterm.core.Core
import com.roversx.thinkterm.core.JumpHost
import com.roversx.thinkterm.core.Notify
import com.roversx.thinkterm.core.ViewSet
import java.io.File
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import java.util.UUID

/// How long a list of a host's threads waits, at least, between reads of
/// the views while the host's terminal is off screen.
private const val LIST_REFRESH_MS = 250L

/// How long a redial goes unsaid (`TerminalModel.redialQuiet`).
private const val QUIET_REDIAL_MS = 2_500L

/// Frames in a row with nothing to draw or read before the frame loop
/// rests (`TerminalModel.tick`): about half a second at 60 Hz.
private const val IDLE_FRAMES_BEFORE_REST = 30

/// How long the connection card waits for a dial back from the background
/// (`TerminalModel.cardHeld`).
private const val CARD_HOLD_MS = 3_000L

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
    private var currentHost by mutableStateOf(host)
    var host: Host
        get() = currentHost
        set(value) {
            val changed = currentHost.autoConnect != value.autoConnect
            currentHost = value
            if (changed) applyAutoReconnect()
        }
    val autoReconnect: Boolean get() = currentHost.autoConnect && settings.autoReconnect

    internal fun applyAutoReconnect() {
        core.setSetting("auto-reconnect", autoReconnect.toString())
    }
    /// A key to approve: the host's own, or (`jump`) that of the jump host
    /// it is reached through, which `endpoint` is then.
    data class HostKeyChallenge(val requestId: String, val endpoint: Host, val fingerprint: String, val jump: Boolean = false)
    var pendingHostKey by mutableStateOf<HostKeyChallenge?>(null)
        private set
    private var activeRequestId: String? = null
    private var requestedHost: Host? = null
    /// The jump host as the request dialled it, if it went through one.
    private var requestedJump: Host? = null


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
    /// The panes of the tab on show, for the switcher under the terminal.
    var panes by mutableStateOf<PanesView?>(null)
        private set
    /// Every pane an agent runs in, for the right sidebar.
    var agents by mutableStateOf<AgentsView?>(null)
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
    /// `toastText` is the App's sticky word on the connection, which the
    /// connection card says itself while it is up.
    var toastSticky by mutableStateOf(false)
        private set
    /// The App's connection is down: redialing by itself, or -- without
    /// `autoReconnect` -- left for the user's retry (`linkIs`).
    var reconnecting by mutableStateOf(false)
        private set
    /// A redial just begun by itself: for its first moments nothing says
    /// so, and one back within them -- the usual one, on the way back to
    /// the app -- leaves the terminal as it was, without a word.
    var redialQuiet by mutableStateOf(false)
        private set
    private val redialSpoken = Runnable { redialQuiet = false }
    /// Dialled again on the way back from the background: for its first
    /// moments the last frame stays in sight with a line over its top, and
    /// the connection card waits. A failure, or the pane, ends it at once.
    var cardHeld by mutableStateOf(false)
        private set
    private val releaseCard = Runnable { cardHeld = false }
    /// The terminal's background, as the App paints it: the chrome around
    /// the terminal takes the same colour.
    var background by mutableStateOf(Color(0xFF1C1C1C))
        private set
    /// The App's font size in points, from its layout view.
    var fontPt by mutableStateOf(11.0)
        private set
    /// The focused pane's place in its scrollback: rows above the bottom,
    /// and rows there are; changes for a moment show the scrollbar.
    var scrollAbove by mutableStateOf(0.0)
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
    /// Something other than the keyboard typed (the key bar, a paste, a
    /// snippet), or the line is another pane's now: a word the keyboard is
    /// making on the terminal as it goes is let go, so its next change does
    /// not land in what came after it.
    var onLineDisturbed: (() -> Unit)? = null
    /// The key panel is up in the keyboard's place.
    var panelOpen by mutableStateOf(false)
    /// A thread this phone was on last time it was in this host, put
    /// back once the threads are listed after a connect.
    private var restoredThread = false
    private val lastThreads = context.getSharedPreferences("thinkterm.threads", Context.MODE_PRIVATE)
    private var appliedIncognito = settings.incognitoKeyboard
    /// The followed preferences as last applied (`followedSettings`).
    private var appliedSettings: List<Any?> = emptyList()
    /// The text size last given the core (`applyFontSize`): one changed from
    /// another screen meanwhile is given on the way back (`enterScreen`).
    private var appliedFontSize = 0.0
    var onScreenChanged: (() -> Unit)? = null
    var onHideKeyboard: (() -> Unit)? = null
    /// The terminal takes the keys and the keyboard comes up.
    var onShowKeyboard: (() -> Unit)? = null
    /// Where the copy and paste chip goes, in dp from the terminal's origin
    /// (a selection's head cell's top-left, or where a right click let
    /// go), or null for none.
    var selectionMenuAt by mutableStateOf<Pair<Double, Double>?>(null)
    /// Whether the chip has something to copy: a right click with nothing
    /// selected offers its Paste alone, its Copy greyed.
    var selectionMenuCopies by mutableStateOf(true)
    /// The pane the chip was opened over, the focused one then
    /// (`openClipChip`).
    private var selectionMenuPane: UInt? = null
    /// The focused pane as the layout last said it.
    private var layoutFocused: Int? = null
    /// The selection was put away, by the chip or by anything else: the
    /// view lets go of the ends it was keeping, so the next tap is a tap
    /// rather than the one that puts a selection away.
    var onSelectionGone: (() -> Unit)? = null

    val smoothScroll: Boolean get() = settings.smoothScroll

    /// Whether anyone sees the terminal: its screen is up, in an app in
    /// front. The core puts a tab zoomed for want of room back to its split
    /// while nobody looks at it here, as iOS tells it.
    private var onScreen = false
    private var inFront = true
    /// The app is in front (not in the background).
    val isInFront: Boolean get() = inFront
    private fun updateShown() = core.setShown(onScreen && inFront)

    /// A list shows this host's threads while its terminal is off screen
    /// (`setViewsListed`): the views are read for it, a few times a second
    /// at most, as the core says they changed -- and not while the app is
    /// in the background, where nobody sees the list.
    private var listed = false
    private val listRefreshQueued = AtomicBoolean(false)
    private val listRefresh = Runnable {
        listRefreshQueued.set(false)
        if (listed && !onScreen && inFront && changePending.getAndSet(false)) refreshViews()
    }

    /// From any thread: a change the list should see is read soon.
    private fun queueListRefresh() {
        if (listRefreshQueued.compareAndSet(false, true)) main.postDelayed(listRefresh, LIST_REFRESH_MS)
    }

    /// The published views read last (`refreshViews`), and the connection
    /// the core was on before the current one was asked for.
    private var lastViews: ViewSet? = null
    private var connectionBefore = 0uL

    private val frameNeeded = AtomicBoolean(false)
    private val changePending = AtomicBoolean(false)
    /// The frame loop rests while there is nothing to draw or read: up while
    /// it rests, and taken by whatever wakes it (`wakeLoop`). Every frame
    /// otherwise wakes the main thread at the display's rate, a frame drawn
    /// or not, which is battery and heat spent on a terminal sitting still.
    private val loopResting = AtomicBoolean(false)
    /// Frames in a row that found nothing to do; main thread only.
    private var idleFrames = 0
    /// The frame callback is posted and not yet run; main thread only, so
    /// a wake and a start never post it twice.
    private var tickPosted = false
    private val main = Handler(Looper.getMainLooper())
    private val choreographer = Choreographer.getInstance()
    /// The frame loop runs: the terminal is on screen in an app in front.
    private var running = false
    private var connected = false
    private val logLines = ArrayDeque<String>()
    private var toastShown: String? = null
    private var appliedFamily = ""

    private val tick = object : Choreographer.FrameCallback {
        override fun doFrame(frameTimeNanos: Long) {
            tickPosted = false
            if (!running) return
            var busy = false
            // Nothing is drawn onto the list's stand-in surface.
            if (!onListSurface && frameNeeded.getAndSet(false)) {
                core.render()
                busy = true
            }
            if (changePending.getAndSet(false)) {
                refreshViews()
                busy = true
            }
            idleFrames = if (busy) 0 else idleFrames + 1
            if (idleFrames < IDLE_FRAMES_BEFORE_REST) {
                postTick()
                return
            }
            // Nothing for a while: the loop stops until the core asks for a
            // frame or publishes views. One asked for between this frame's
            // look and the flag going up found the loop awake and did not
            // wake it: looked at again now that the flag is up.
            idleFrames = 0
            loopResting.set(true)
            if ((!onListSurface && frameNeeded.get()) || changePending.get()) wakeLoop()
        }
    }

    private fun postTick() {
        if (tickPosted) return
        tickPosted = true
        choreographer.postFrameCallback(tick)
    }

    /// From any thread: the loop resting is set going again, once.
    private fun wakeLoop() {
        if (!loopResting.compareAndSet(true, false)) return
        main.post { if (running) postTick() }
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
        updateShown()
        applyScheme()
        applyScrollMode()
        applyTerminalPrefs()
        applyFontSize()
        applyAutoReconnect()
        appliedFamily = settings.fontFamily
        appliedSettings = followedSettings()
    }

    /// The preferences the terminal follows, as one value: what the screen
    /// watches, and what a session off screen meanwhile catches up on.
    fun followedSettings(): List<Any?> = listOf(
        settings.schemeName, settings.smoothScroll, settings.cursorStyle, settings.cursorBlink,
        settings.contrast, settings.resizeMode, settings.paneBars, settings.fontFamily,
        settings.incognitoKeyboard, settings.language,
    )

    /// The preferences the open terminal follows: the screen calls this
    /// when any of them changes (a snapshot read in a composable), and
    /// coming back on screen when they changed while it was away.
    fun settingsChanged() {
        appliedSettings = followedSettings()
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
            if (host.autoConnect && connection.startsWith("pane ")) {
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
    /// The thread on show is written down for the next connect; the first
    /// listing after a connect puts the phone back on the one it left.
    private fun rememberThread(threads: ThreadsView?) {
        val list = threads?.threads ?: return
        val current = list.firstOrNull { it.current }?.id ?: return
        val key = "last." + host.id
        if (!restoredThread) {
            restoredThread = true
            val saved = lastThreads.getString(key, null)
            if (saved != null && saved != current && list.any { it.id == saved }) {
                openThread(saved, null)
                return
            }
        }
        if (lastThreads.getString(key, null) != current) lastThreads.edit().putString(key, current).apply()
    }

    /// The screen went away but the connection stays: the frame loop
    /// rests until the screen is back, and the views catch up then.
    fun leaveScreen() {
        onScreen = false
        updateShown()
        updateRunning()
        if (listed) queueListRefresh()
    }

    fun enterScreen() {
        onScreen = true
        updateShown()
        updateRunning()
        // Changed from another screen while this one was away.
        if (followedSettings() != appliedSettings) settingsChanged()
        if (settings.fontSize != appliedFontSize) applyFontSize()
    }

    /// The frame loop runs while the terminal is on screen in an app in
    /// front, and rests otherwise; the views catch up when it starts.
    private fun updateRunning() {
        val want = onScreen && inFront
        if (want == running) return
        running = want
        if (want) {
            loopResting.set(false)
            idleFrames = 0
            postTick()
            main.removeCallbacks(statsTick)
            main.postDelayed(statsTick, 500)
            changePending.set(true)
        } else {
            choreographer.removeFrameCallback(tick)
            tickPosted = false
            // Stopped, not resting: nothing is to start it but coming back.
            loopResting.set(false)
            main.removeCallbacks(statsTick)
        }
    }

    /// A host as edited, onto its session. A changed connection that was up
    /// is made again on the edit, as is one the terminal's own Save asks
    /// for; that Save with nothing changed retries one that is down. Each
    /// is the user's tap or a session they had open, not a connection made
    /// by itself, so the host's `autoConnect` has no say in it. One lost and
    /// left for the user's retry is down, though its status still names the
    /// pane: an edit elsewhere is not that retry.
    fun applyHostEdit(saved: Host, credentialsChanged: Boolean, connectOnSave: Boolean) {
        val changed = !host.sameSshConnection(saved) || credentialsChanged
        val lost = reconnecting && !autoReconnect
        val needsRetry = when (ConnectionPhase.of(connection)) {
            is ConnectionPhase.Failed, ConnectionPhase.Disconnected -> true
            null -> lost
            else -> false
        }
        val redial = if (changed) (isConnected && !lost) || connectOnSave else connectOnSave && needsRetry
        if (changed) disconnect()
        host = saved
        if (redial) connect()
    }

    fun connect() {
        // A dial of its own: whoever holds the card back holds it for this
        // one, after the call (`enteredForeground`).
        endCardHold()
        pausedForBackground = false
        clearHostKeyRequest()
        val requestId = UUID.randomUUID().toString()
        activeRequestId = requestId
        requestedHost = host
        restoredThread = false
        // A new connection's views are all new, even ones that read the
        // same; the core may still have the last one's out until it is up.
        lastViews = null
        connectionBefore = core.viewsConnection()
        val h = host
        // One whose jump host is gone is not dialled some other way.
        val jump = store.jumpOf(h)
        if (h.jumpHost != null && jump == null) {
            connected = false
            connection = "failed: " + tr("jump.missing")
            return
        }
        requestedJump = jump
        connected = true
        val secret = if (h.id == Host.PROBE_ID) probeKey() else store.secret(h.id)
        val passphrase = if (h.id == Host.PROBE_ID) null else store.passphrase(h.id)
        log("shell: connecting ${h.address}" + (jump?.let { " via ${it.address}" } ?: "") + ", ${secret.length} chars of secret")
        core.connect(
            requestId = requestId,
            host = h.hostname,
            port = h.port.coerceIn(1, 65535).toUShort(),
            user = h.user,
            authKind = h.auth,
            secret = secret,
            passphrase = passphrase,
            knownHost = h.knownHost,
            jump = jump?.let { j ->
                JumpHost(
                    host = j.hostname,
                    port = j.port.coerceIn(1, 65535).toUShort(),
                    user = j.user,
                    authKind = j.auth,
                    secret = store.secret(j.id),
                    passphrase = store.passphrase(j.id),
                    knownHost = j.knownHost,
                )
            },
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
        endCardHold()
        clearHostKeyRequest()
        connected = false
        core.disconnect()
    }

    private fun clearHostKeyRequest() {
        activeRequestId = null
        requestedHost = null
        requestedJump = null
        pendingHostKey = null
    }

    private fun currentHostKeyEndpoint(requestId: String): Host? =
        requestedHost?.takeIf { activeRequestId == requestId && host.knownHost == null && host.sameSshConnection(it) }

    /// The jump host this request dialled, as the store has it now -- with
    /// the key pinned for it since, if one was -- while it is reached the
    /// same way. The host's own key, pinned or not, has no say in it.
    private fun currentJumpEndpoint(requestId: String): Host? {
        val asked = requestedJump?.takeIf { activeRequestId == requestId } ?: return null
        return store.hosts.firstOrNull { it.id == asked.id }?.takeIf { it.sameJumpWay(asked) }
    }

    /// The key a host's question is about was pinned in the store since the
    /// dial was made: approved in another session -- the jump host's own, or
    /// one through it, or the other way about. The dial is made again with
    /// it, rather than the question put twice or left unasked.
    private fun pinnedSince(endpoint: Host): Boolean {
        val pinned = store.hosts.firstOrNull { it.id == endpoint.id }?.knownHost ?: return false
        if (endpoint.id == host.id) host = host.copy(knownHost = pinned)
        connect()
        return true
    }

    /// The host's pinned key forgotten: the connection up now stays, and
    /// the next dial -- the core's own redial of this one too -- asks for
    /// the key the host has now.
    fun forgetHostKey() {
        host = host.copy(knownHost = null)
        // That redial is still this request's, now without its pin: the
        // question it asks is this one's to put to the user.
        requestedHost = requestedHost?.copy(knownHost = null)
        core.forgetHostKey()
    }

    /// As `forgetHostKey`, for the jump host this one is reached through.
    fun forgetJumpHostKey() {
        requestedJump = requestedJump?.copy(knownHost = null)
        core.forgetJumpHostKey()
    }

    /// The failure card's "forget the key and retry": the key that changed
    /// is the jump host's when the failure says so. Either way it goes from
    /// every session that holds it, the ones reached through it too.
    fun forgetKeyAndRetry(reason: String) {
        val jump = store.jumpOf(host)
        if (jump != null && reason.contains("the jump host")) {
            com.roversx.thinkterm.forgetHostKey(store, jump.id)
        } else {
            com.roversx.thinkterm.forgetHostKey(store, host.id)
            host = host.copy(knownHost = null)
        }
        connect()
    }

    /// The jump host this host is reached through was edited: a session up
    /// through it is made again the new way, as one is on an edit of its
    /// own host. One lost and left for the user's retry stays so.
    fun applyJumpHostEdit() {
        val lost = reconnecting && !autoReconnect
        val redial = isConnected && !lost
        disconnect()
        if (redial) connect()
    }

    fun approveHostKey(challenge: HostKeyChallenge) {
        if (challenge.jump) {
            val now = currentJumpEndpoint(challenge.requestId)?.takeIf { it.sameJumpWay(challenge.endpoint) } ?: return
            // One pinned meanwhile, in another session, is the one dialled
            // with: a key it does not match is the failure's to say.
            if (now.knownHost == null) store.rememberHostKey(now.id, challenge.fingerprint)
            connect()
            return
        }
        if (currentHostKeyEndpoint(challenge.requestId)?.sameSshConnection(challenge.endpoint) != true) return
        if (host.id != Host.PROBE_ID) store.rememberHostKey(host.id, challenge.fingerprint)
        host = host.copy(knownHost = challenge.fingerprint)
        connect()
    }

    val isConnected: Boolean
        get() = connection.isNotEmpty() && !connection.startsWith("disconnected") && !connection.startsWith("failed")

    /// What keeps connections up while the app is away is for this one
    /// (`Sessions.connectionsChanged`): attached, or redialing by itself
    /// (the status names the pane all the while), or on its way in -- asked
    /// for on screen, it may only come up once the app is away. One lost
    /// and left for the user's retry holds nothing up.
    val keepsUp: Boolean
        get() = if (connection.startsWith("pane ")) !(reconnecting && !autoReconnect)
        else connection.startsWith("connecting") || connection.startsWith("connected")

    /// Leaving the screen for good: the frame loop stops, then the core.
    fun shutdown() {
        clearHostKeyRequest()
        dropListSurface()
        main.removeCallbacks(pollUploads)
        // Their copies, which nothing reads once the core is down.
        for (upload in uploads) if (upload.ended == null) upload.file.parentFile?.deleteRecursively()
        copier.shutdown()
        running = false
        listed = false
        choreographer.removeFrameCallback(tick)
        main.removeCallbacks(statsTick)
        main.removeCallbacks(listRefresh)
        core.disconnect()
        core.shutdown()
    }

    /// A photo or file picker of this screen's is up: the system's own
    /// screen over the app, not the app put away. What is picked goes up
    /// on this connection, so the setting below does not drop it meanwhile.
    var picking = false

    /// The app left the screen. The socket lives as long as the process
    /// does; with the setting off, it is dropped now and redialed on return.
    fun enteredBackground() {
        inFront = false
        updateShown()
        updateRunning()
        // Attached, or on its way (a failed dial waiting to be made again
        // reads "connecting" too): dropped now, and dialled again on return.
        val wanted = connection.startsWith("pane ") || connection.startsWith("connecting")
        if (!settings.keepSessionInBackground && !picking && wanted) {
            disconnect()
            pausedForBackground = true
        }
    }

    fun enteredForeground() {
        inFront = true
        updateShown()
        updateRunning()
        if (listed && !onScreen) queueListRefresh()
        // Only what the background took down comes back: a session the
        // user disconnected stays so.
        if (pausedForBackground) {
            pausedForBackground = false
            if (host.autoConnect) {
                connect()
                holdCard()
            }
        } else {
            checkConnection()
        }
    }

    /// What a wait or a network change cut is found out now, and what was
    /// already down is tried again at once.
    fun checkConnection() = core.checkConnection()

    /// The background took the connection down; the foreground brings it back.
    /// Meanwhile the connection page stays away: a "disconnected" of the
    /// app's own making is not one to show, in the recent apps' picture of
    /// it or as the app comes back (`connectionCovers`).
    var pausedForBackground by mutableStateOf(false)
        private set

    /// See `cardHeld`.
    private fun holdCard() {
        main.removeCallbacks(releaseCard)
        cardHeld = true
        main.postDelayed(releaseCard, CARD_HOLD_MS)
    }

    private fun endCardHold() {
        main.removeCallbacks(releaseCard)
        cardHeld = false
    }

    // MARK: input

    fun key(name: String, ctrl: Boolean = false, alt: Boolean = false, shift: Boolean = false) {
        // A sticky modifier is spent by the next key whether or not that
        // key brought the modifier itself, so it never lingers past it.
        val stickyCtrl = takeCtrl()
        val stickyAlt = takeAlt()
        onLineDisturbed?.invoke()
        core.key(name, ctrl || stickyCtrl, alt || stickyAlt, shift)
    }

    /// Text from the keyboard. A sticky Ctrl or Alt turns a single
    /// character into a chord instead.
    fun text(text: String) {
        if ((ctrlSticky || altSticky) && text.length == 1) {
            key(text)
            return
        }
        onLineDisturbed?.invoke()
        core.text(text)
    }

    fun pasteFromClipboard() {
        val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as? ClipboardManager ?: return
        val text = cm.primaryClip?.getItemAt(0)?.coerceToText(context)?.toString() ?: return
        paste(text)
    }

    /// `text` into the pane as a paste rather than as typing: a program
    /// that asked for pastes bracketed gets it so, and does not run each
    /// line as it arrives.
    fun paste(text: String) {
        if (text.isEmpty()) return
        onLineDisturbed?.invoke()
        core.paste(text)
    }

    /// The copy and paste chip, at `x`, `y` in dp from the terminal's
    /// origin, over the pane focused now: the one its Copy and Paste act
    /// on. `copies`: whether a selection is there for its Copy.
    fun openClipChip(x: Double, y: Double, copies: Boolean) {
        selectionMenuCopies = copies
        selectionMenuPane = core.focusedPane()
        selectionMenuAt = Pair(x, y)
    }

    /// Whether the chip still stands over the focused pane. Another one
    /// focused since -- a tab picked, its pane gone -- takes the chip away
    /// instead: its Paste is not for a pane it was not opened over.
    private fun chipOverFocus(): Boolean {
        if (selectionMenuPane != null && core.focusedPane() == selectionMenuPane) return true
        selectionMenuAt = null
        return false
    }

    /// The chip's Paste: the clipboard into the pane, and the chip put away
    /// with any selection it stood over, as its Copy puts them away.
    fun pasteFromMenu() {
        if (!chipOverFocus()) return
        clearSelection()
        pasteFromClipboard()
    }

    /// The chip's Copy.
    fun copyFromMenu() {
        if (chipOverFocus()) copySelection()
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

    /// A pinch's step, and the settings' live text size: a point on the
    /// text size every pane takes, kept in the settings, so the next
    /// connection and the next launch start at it too. (The core's own
    /// step, the desktop's Cmd+= for one pane, lives only as long as the
    /// connection; on a phone a pinch is the text size.)
    fun stepFont(by: Double) {
        val size = Math.round(settings.fontSize + by).toDouble().coerceIn(6.0, 40.0)
        if (size == settings.fontSize) return
        settings.fontSize = size
        applyFontSize()
    }

    /// The text size every pane takes, pinned, as iOS's. Apart from the
    /// rest of the settings (`settingsChanged`), which a pinch's every step
    /// would otherwise send again.
    private fun applyFontSize() {
        appliedFontSize = settings.fontSize
        core.setSetting("font", "{\"mode\":\"pinned\",\"pt\":${settings.fontSize}}")
    }

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
        onLineDisturbed?.invoke()
        core.chromeClick(action, pane?.toUInt(), tab?.toUInt())
    }

    fun sideClick(kind: String, id: String? = null, flag: Boolean? = null) {
        onLineDisturbed?.invoke()
        core.sideClick(kind, id, flag)
    }

    fun sideKey(key: String, value: String) = core.sideKey(key, value)

    /// Show a thread, switching the Space on show to its own first when
    /// it is in another one, so the strips follow.
    fun openThread(id: String, spaceId: String?) {
        if (spaceId != null && tree?.spaces?.firstOrNull { it.current }?.id != spaceId) core.setSpace(spaceId)
        sideClick("thread", id)
    }

    fun setSpace(id: String) = core.setSpace(id)

    /// One pane of a tab at a time, whole, where the screen has no room
    /// for a split's panes side by side.
    fun setOnePane(one: Boolean) = core.setOnePane(one)

    fun contextMenu(kind: String, id: String): List<MenuItem> = Views.menu(core.contextMenu(kind, id))

    /// Run a menu row. Copy and paste are the shell's to do.
    fun menuAction(id: String) {
        val outcome = Views.menuOutcome(core.menuAction(id)) ?: return
        outcome.copy?.let { copyToClipboard(it) }
        if (outcome.paste) pasteFromClipboard()
    }

    fun takeOver() = core.takeOver()

    /// Open or fold the sub-task list of the agent in `pane`.
    fun toggleSubtasks(pane: Int) = core.agentToggleSubtasks(pane.toUInt())

    // MARK: an agent's conversation as a chat

    /// Whom the chat on show is with: the pane, the agent as its detection
    /// names it, and where it is ("Project · Thread").
    data class ChatTarget(val pane: Int, val agent: String, val place: String)

    /// The chat on show, over the terminal of its agent's pane; null when
    /// none is (`chat` is null too until its first view comes).
    var chatWith by mutableStateOf<ChatTarget?>(null)
        private set
    var chat by mutableStateOf<ChatView?>(null)
        private set

    /// The chat on show was opened from Threads: going back from it goes
    /// back there, past the terminal under it.
    var chatFromThreads = false
        private set
    /// Bumped to send the terminal's screen back to Threads, a chat opened
    /// there still over it: `TerminalScreen` goes, and closes the chat once
    /// it is gone.
    var backToThreads by mutableIntStateOf(0)
        private set

    /// Show `row`'s agent as a chat. The terminal under it goes to the
    /// agent's pane first, as a tap on the row always did: going back from
    /// the chat is going to that terminal.
    fun openChat(row: AgentRow) = openChat(row.pane, row.agentId, row.place.ifEmpty { row.name })

    /// `fromThreads`: going back from the chat goes back to Threads rather
    /// than to the terminal under it.
    fun openChat(pane: Int, agent: String, place: String, fromThreads: Boolean = false) {
        chromeClick("pane", pane = pane)
        chat = null
        chatThumbs.clear()
        chatFromThreads = fromThreads
        chatWith = ChatTarget(pane, agent, place.ifEmpty { agent })
        core.chatOpen(pane.toUInt(), agent)
    }

    /// The agent a notification was about: its chat when it has one, else
    /// its pane.
    fun showAgent(link: AgentLink) {
        if (link.chat) {
            openChat(link.pane, link.agent, link.place)
        } else {
            closeChat(toTerminal = true)
            chromeClick("pane", pane = link.pane)
        }
    }

    /// Back from the chat: to the terminal under it -- or, for one opened
    /// from Threads, to Threads, unless `toTerminal` (an ask answered only
    /// in the terminal).
    fun closeChat(toTerminal: Boolean = false) {
        if (chatWith == null) return
        if (chatFromThreads && !toTerminal) {
            backToThreads++
            return
        }
        chatFromThreads = false
        chatWith = null
        chat = null
        core.chatClose()
        for (file in chatFiles.toList()) chatDetach(file)
    }

    // MARK: files sent in the chat

    /// A file picked to go with what is said next: on its way to the host's
    /// uploads folder, there (`path`), or not (`error`).
    data class ChatFile(
        val id: ULong,
        val name: String,
        val image: Boolean,
        /// Its size, in bytes.
        val size: Long,
        /// A small picture of it, for a picture.
        val preview: ImageBitmap?,
        val sent: ULong = 0uL,
        val total: ULong = 0uL,
        val path: String? = null,
        val error: String? = null,
    )

    /// What goes with what is said next.
    val chatFiles = mutableStateListOf<ChatFile>()

    /// The phone's own copies of the pictures sent, by where they are on
    /// the host: what a tap shows big, and what shows before the host's
    /// small picture comes.
    val chatPictures = mutableStateMapOf<String, File>()

    /// The app's copy of each file picked, by upload, until it is sent.
    private val chatCopies = HashMap<ULong, File>()

    /// The host's small pictures of what was sent, by the key the chat's
    /// rows name them by: each comes once ("chat_picture").
    val chatThumbs = mutableStateMapOf<String, ImageBitmap>()

    /// Put a file the phone has -- a photo, a file, the clipboard's
    /// picture -- in the host's uploads folder, to go with what is said
    /// next. `write` fills the app's own copy, off the main thread.
    fun chatAttach(name: String, image: Boolean, write: (File) -> Unit) {
        val clean = name.ifEmpty { "file" }.replace('/', '_')
        val id = core.uploadBegin()
        copier.execute {
            ChatFilesFolder.clearOnce(context.cacheDir)
            val folder = File(context.cacheDir, "chat-files/" + UUID.randomUUID())
            val file = File(folder, clean)
            val made = runCatching {
                folder.mkdirs()
                write(file)
                file.exists()
            }.getOrDefault(false)
            val preview = if (made && image) runCatching { pictureOf(file, 360) }.getOrNull() else null
            main.post {
                if (!made) {
                    folder.deleteRecursively()
                    core.cancelUpload(id)
                    note(tr("upload.unread", clean))
                } else {
                    core.uploadFile(id, clean, file.path)
                    chatCopies[id] = file
                    chatFiles.add(ChatFile(id, clean, image, file.length(), preview))
                    main.removeCallbacks(pollChatFiles)
                    main.postDelayed(pollChatFiles, 150)
                }
            }
        }
    }

    /// Take a picked file off what goes next; one still on its way is
    /// given up.
    fun chatDetach(file: ChatFile) {
        if (file.path == null && file.error == null) core.cancelUpload(file.id)
        chatFiles.removeAll { it.id == file.id }
        chatCopies.remove(file.id)?.parentFile?.deleteRecursively()
    }

    private val pollChatFiles: Runnable = object : Runnable {
        override fun run() {
            var going = false
            for (i in chatFiles.indices) {
                val file = chatFiles[i]
                if (file.path != null || file.error != null) continue
                val status = core.uploadStatus(file.id)
                if (status == null) {
                    chatFiles[i] = file.copy(error = "cancelled")
                    continue
                }
                var next = file.copy(sent = status.sent, total = status.total)
                if (status.done) {
                    // Its path goes with what is said, not into a pane.
                    core.uploadUnpasted(file.id)
                    next = if (status.error.isEmpty()) next.copy(path = status.path) else next.copy(error = status.error)
                } else {
                    going = true
                }
                if (next != file) chatFiles[i] = next
            }
            if (going) main.postDelayed(this, 150)
        }
    }

    /// Say `text`, with the files picked that have reached the host.
    fun chatSend(text: String) {
        val ready = chatFiles.filter { it.path != null }
        for (file in ready) {
            val copy = chatCopies.remove(file.id) ?: continue
            if (file.image) chatPictures[file.path!!] = copy else copy.parentFile?.deleteRecursively()
        }
        chatFiles.removeAll { it.path != null }
        core.chatSend(text, ready.map { ChatAttachment(it.name, it.image, it.path!!) })
    }

    /// A command picked from the field's list, sent as typed: the files
    /// picked wait for what is said next.
    fun chatCommand(text: String) = core.chatSend(text, emptyList())

    fun chatSetModel(model: String, effort: String?) = core.chatSetModel(model, effort)
    fun chatAnswer(ask: String, choice: String) = core.chatAnswer(ask, choice)
    fun chatInterrupt() = core.chatInterrupt()
    fun chatOlder() = core.chatOlder()
    fun chatAction(action: String) = core.chatAction(action)
    fun chatToggle(row: String) = core.chatToggle(row)

    /// The selected text to the clipboard, and the selection put away.
    fun copySelection() {
        core.selectedText()?.takeIf { it.isNotEmpty() }?.let { copyToClipboard(it) }
        clearSelection()
    }

    fun clearSelection() {
        core.clearSelection()
        selectionMenuAt = null
        onSelectionGone?.invoke()
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
        // The views the core published, read without waiting on it; only
        // what changed since the last read is decoded.
        val last = lastViews
        if (last != null && core.viewsRevision() == last.revision) return
        val views = core.views()
        if (views.revision == last?.revision) return
        lastViews = views
        if (views.tabs != last?.tabs) {
            val tabs = Views.tabs(views.tabs)
            if (tabs != this.tabs) this.tabs = tabs
        }
        if (views.sidebar != last?.sidebar) {
            val sidebar = Views.sidebar(views.sidebar)
            if (sidebar != this.sidebar) this.sidebar = sidebar
        }
        val threadsChanged = views.threads != last?.threads
        if (threadsChanged) {
            val threads = Views.threads(views.threads)
            if (threads != this.threads) this.threads = threads
        }
        // Only this connection's threads say which one to go back to.
        if (views.connection > connectionBefore && (threadsChanged || !restoredThread)) rememberThread(this.threads)
        if (views.tree != last?.tree) {
            val tree = Views.tree(views.tree)
            if (tree != this.tree) this.tree = tree
        }
        if (views.navs != last?.navs) {
            val navs = Views.navs(views.navs)
            if (navs != this.navs) this.navs = navs
        }
        if (views.panes != last?.panes) {
            val panes = Views.panes(views.panes)
            if (panes != this.panes) this.panes = panes
        }
        if (views.agents != last?.agents) {
            val agents = Views.agents(views.agents)
            if (agents != this.agents) this.agents = agents
        }
        if (views.status != last?.status) {
            val status = Views.status(views.status)
            if (status != this.status) {
                this.status = status
                showToast(status?.toast)
            }
        }
        if (views.layout != last?.layout) Views.layout(views.layout)?.let { layout ->
            // Another pane focused takes away the chip opened over the
            // last. Only a change counts: a layout the core published
            // before the right click that focused the chip's own pane
            // still names the pane before it.
            if (layout.focused != layoutFocused) {
                layoutFocused = layout.focused
                if (selectionMenuAt != null && layout.focused.toUInt() != selectionMenuPane) selectionMenuAt = null
            }
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

    /// The App's connection went down or came back, as the core says it
    /// on screen or off (`onPublished`'s "link"): the views that also say
    /// so are read only while someone looks, and what keeps the connection
    /// up while the app is away goes by it.
    private fun linkIs(down: Boolean) {
        if (reconnecting == down) return
        log(if (down) "link: down" else "link: up")
        reconnecting = down
        main.removeCallbacks(redialSpoken)
        redialQuiet = down && autoReconnect
        if (redialQuiet) main.postDelayed(redialSpoken, QUIET_REDIAL_MS)
        Sessions.connectionsChanged()
    }

    private fun showToast(toast: Toast?) {
        main.removeCallbacks(expireToast)
        val sticky = toast?.sticky ?: false
        if (toast == null || toast.text.isEmpty()) {
            if (toastText != null) toastText = null
            return
        }
        toastText = toast.text
        toastSticky = sticky
        toastShown = toast.text
        if (!sticky) main.postDelayed(expireToast, 4000)
    }

    private val expireToast = Runnable { if (toastText == toastShown) toastText = null }

    // MARK: uploads

    /// A file on its way to the host, or one that just got there or did
    /// not: shown over the keys until it has said so.
    data class Upload(
        val id: ULong,
        val name: String,
        /// The app's own copy, read as it is sent and removed after.
        val file: File,
        val sent: ULong = 0uL,
        val total: ULong = 0uL,
        /// Where it is on the host, once it is.
        val path: String? = null,
        val error: String? = null,
        val ended: Long? = null,
    )

    val uploads = mutableStateListOf<Upload>()
    private val copier = Executors.newSingleThreadExecutor()

    /// Leave something the phone has -- a photo, a file, the clipboard's
    /// picture -- in the host's uploads folder and put its path on the
    /// focused pane's command line, as the desktop does with a file dropped
    /// on a remote terminal. `write` fills the app's own copy, off the main
    /// thread, which the upload then reads at its own pace. The pane is the
    /// one focused now, as it is chosen, however long the copy takes.
    fun upload(name: String, write: (File) -> Unit) {
        val clean = name.ifEmpty { "file" }.replace('/', '_')
        val id = core.uploadBegin()
        copier.execute {
            UploadsFolder.clearOnce(context.cacheDir)
            val folder = File(context.cacheDir, "uploads/" + UUID.randomUUID())
            val file = File(folder, clean)
            val made = runCatching {
                folder.mkdirs()
                write(file)
                file.exists()
            }.getOrDefault(false)
            main.post {
                if (!made) {
                    folder.deleteRecursively()
                    core.cancelUpload(id)
                    note(tr("upload.unread", clean))
                } else {
                    startUpload(id, clean, file)
                }
            }
        }
    }

    /// Give an upload up: the host keeps nothing of it.
    fun cancelUpload(id: ULong) = core.cancelUpload(id)

    private fun startUpload(id: ULong, name: String, file: File) {
        core.uploadFile(id, name, file.path)
        uploads.add(Upload(id, name, file))
        main.removeCallbacks(pollUploads)
        main.postDelayed(pollUploads, 150)
    }

    /// How each upload stands, as the core has it; one that is done pastes
    /// its path and stays a moment to say so, one that failed a little
    /// longer to say why, and one given up goes at once.
    private val pollUploads: Runnable = object : Runnable {
        override fun run() {
            for (i in uploads.indices) {
                val upload = uploads[i]
                if (upload.ended != null) continue
                val status = core.uploadStatus(upload.id)
                if (status == null) {
                    upload.file.parentFile?.deleteRecursively()
                    uploads[i] = upload.copy(ended = SystemClock.uptimeMillis())
                    continue
                }
                var next = upload.copy(sent = status.sent, total = status.total)
                if (status.done) {
                    next = next.copy(ended = SystemClock.uptimeMillis())
                    upload.file.parentFile?.deleteRecursively()
                    if (status.error.isEmpty()) {
                        next = next.copy(path = status.path)
                        // Into the pane it was started from, not
                        // whichever has the focus now.
                        onLineDisturbed?.invoke()
                        core.pasteUpload(upload.id, status.paste)
                    } else {
                        next = next.copy(error = status.error)
                    }
                }
                if (next != upload) uploads[i] = next
            }
            val now = SystemClock.uptimeMillis()
            uploads.removeAll { u ->
                val ended = u.ended ?: return@removeAll false
                u.error == "cancelled" || now - ended > (if (u.error == null) 1500 else 5000)
            }
            if (uploads.isNotEmpty()) main.postDelayed(this, 150)
        }
    }

    /// A passing remark of the shell's own, where the App's go.
    fun note(text: String) {
        main.removeCallbacks(expireToast)
        toastText = text
        toastSticky = false
        toastShown = text
        main.postDelayed(expireToast, 4000)
    }

    // MARK: listed with the other hosts

    /// The surface lent for the list, and whether the core is on it.
    private var listReader: ImageReader? = null
    private var listWindow = 0L
    private var onListSurface = false

    /// The list of every host's threads shows this one's, or stopped:
    /// while it does the core keeps the views current with the terminal
    /// off screen, and the list reads them (`refreshViews`).
    ///
    /// The core attaches to the server -- and has threads to list -- only
    /// once it has a surface to draw on, so a host listed while its
    /// terminal is off screen is lent one of the screen's size that
    /// nothing shows: an ImageReader that drops whatever reaches it. The
    /// terminal's own takes its place when it comes (`dropListSurface`),
    /// as iOS lends a layer nothing shows.
    fun setViewsListed(listed: Boolean) {
        this.listed = listed
        core.setViewsListed(listed)
        if (listed && !onScreen) {
            changePending.set(true)
            queueListRefresh()
        }
        if (listed && generation == 0uL) {
            val metrics = context.resources.displayMetrics
            val width = metrics.widthPixels
            val height = metrics.heightPixels
            val reader = listReader ?: ImageReader.newInstance(
                width, height, PixelFormat.RGBA_8888, 2,
                HardwareBuffer.USAGE_GPU_COLOR_OUTPUT or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE,
            ).also {
                it.setOnImageAvailableListener({ r -> r.acquireLatestImage()?.close() }, main)
                listReader = it
            }
            if (listWindow == 0L) listWindow = NativeWindow.fromSurface(reader.surface)
            val gen = core.attachSurface(listWindow.toULong(), width.toUInt(), height.toUInt(), metrics.density.toDouble())
            generation = gen
            // Refused, the surface is kept for the list's next try.
            onListSurface = gen != 0uL
        } else if (!listed) {
            dropListSurface()
        }
    }

    /// The list's surface given back: the terminal's own is coming, or the
    /// list has gone.
    fun dropListSurface() {
        if (onListSurface && generation != 0uL) {
            core.detachSurface(generation)
            generation = 0uL
        }
        onListSurface = false
        if (listWindow != 0L) {
            NativeWindow.release(listWindow)
            listWindow = 0L
        }
        listReader?.close()
        listReader = null
        // The loop may have rested while frames went to the list's surface,
        // with one still owed: it is owed to this surface now.
        wakeLoop()
    }

    // MARK: the preferences the App honours

    /// The phone is in its light appearance.
    private val systemLight: Boolean
        get() = (context.resources.configuration.uiMode and Configuration.UI_MODE_NIGHT_MASK) != Configuration.UI_MODE_NIGHT_YES
    /// The appearance the scheme was last chosen for.
    private var schemeForLight: Boolean? = null

    /// The phone may have gone light or dark meanwhile.
    fun appearanceMayHaveChanged() {
        if (schemeForLight != systemLight) applyScheme()
    }

    /// The scheme the user picked; with none picked, the host's own -- and
    /// on a light phone the light default in place of a host's that is no
    /// choice at all, as the desktop decides its own
    /// (`native_settings::resolve_color_scheme`): a scheme the host's
    /// configuration names is kept on either side.
    private fun applyScheme() {
        val name = settings.schemeName
        val json = Schemes.json(name)
        val light = systemLight
        schemeForLight = light
        if (name == Schemes.FOLLOW_DESKTOP || json == null) {
            core.setSetting("terminal-scheme", "\"desktop\"")
            core.setPalette(null)
            val lightScheme = if (light) Schemes.json(Schemes.LIGHT_DEFAULT) else null
            core.setFallbackPalette(lightScheme, lightScheme?.let { Schemes.json(Schemes.DARK_DEFAULT) })
        } else {
            core.setSetting("terminal-scheme", "\"" + name.replace("\"", "\\\"") + "\"")
            core.setPalette(json)
            core.setFallbackPalette(null, null)
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
        core.setSetting("pane-bars", settings.paneBars.toString())
        // The core's own menus and messages follow the app's language.
        core.setSetting("language", "\"${L10n.currentTag(settings.language)}\"")
    }

    // MARK: the log

    // Pane titles, remote stderr and user@host pass through here; logcat
    // is readable by other apps with the right permission and by anyone
    // with adb, so only a debuggable build writes it there. The in-app
    // log view keeps the lines either way.
    private val debuggable =
        (context.applicationInfo.flags and android.content.pm.ApplicationInfo.FLAG_DEBUGGABLE) != 0

    fun log(line: String) {
        if (debuggable) Log.i("thinkterm", line)
        logLines.addLast(line)
        while (logLines.size > 60) logLines.removeFirst()
        logText = logLines.joinToString("\n")
    }

    // MARK: what the core tells the shell (core thread)

    private inner class Sink : Notify {
        override fun onFrameNeeded() {
            frameNeeded.set(true)
            wakeLoop()
        }

        override fun onStatus(status: String) {
            main.post {
                connection = status
                Sessions.connectionsChanged()
                log("status: $status")
                if (status.startsWith("pane ")) {
                    attached = true
                    if (host.id != Host.PROBE_ID) store.touchConnected(host.id)
                }
                // Back on the pane, or stopped: nothing is left to wait for.
                // A plain "disconnected" may be the background's own, landing
                // after the dial back began, so it does not end the wait.
                if (status.startsWith("pane ") || ConnectionPhase.of(status) is ConnectionPhase.Failed) {
                    endCardHold()
                }
                if (status.startsWith("connecting") || status.contains("reconnect") || status.startsWith("disconnected")) {
                    composing = null
                    // The chip stood over a pane of the connection that went.
                    selectionMenuAt = null
                    onDropComposition?.invoke()
                }
                changePending.set(true)
                wakeLoop()
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
            wakeLoop()
            // Off screen the frame loop rests; a list may be reading.
            queueListRefresh()
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

        override fun onHostKeyRequired(requestId: String, fingerprint: String, jump: Boolean) {
            main.post {
                val endpoint = (if (jump) currentJumpEndpoint(requestId) else currentHostKeyEndpoint(requestId)) ?: return@post
                if (endpoint.id != Host.PROBE_ID && pinnedSince(endpoint)) return@post
                pendingHostKey = HostKeyChallenge(requestId, endpoint, fingerprint, jump)
            }
        }

        override fun onPublished(key: String, value: String) {
            when (key) {
                "bg" -> {
                    val color = parseHex(value) ?: return
                    main.post { if (background != color) background = color }
                }
                "link" -> main.post { linkIs(down = value == "down") }
                "chat_picture" -> {
                    // Each small picture once, decoded here, off the screen's thread.
                    val picture = runCatching {
                        val o = org.json.JSONObject(value)
                        val bytes = android.util.Base64.decode(o.getString("data"), android.util.Base64.DEFAULT)
                        val bitmap = android.graphics.BitmapFactory.decodeByteArray(bytes, 0, bytes.size)?.asImageBitmap()
                        Triple(o.getInt("pane"), o.getString("key"), bitmap)
                    }.getOrNull()
                    if (picture != null) {
                        val (pane, key, bitmap) = picture
                        if (bitmap != null) main.post { if (chatWith?.pane == pane) chatThumbs[key] = bitmap }
                    }
                }
                "chat" -> {
                    val parsed = ChatViews.parse(value)
                    // Only the chat on show: a late view of the one before
                    // is dropped.
                    main.post { if (parsed != null && chatWith?.pane == parsed.pane) chat = parsed }
                }
                "agent_waiting" -> main.post { AgentAlerts.waiting(context, host, value) }
                "agent_resumed" -> main.post { AgentAlerts.resumed(context, host, value) }
            }
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

/// The uploads folder as a process finds it holds only what one before it
/// left, stopped in the middle of an upload: cleared once, before this
/// one's first upload makes its own copy there.
/// The app's copies of the files picked for a chat, from a run before
/// this one: gone once, before the first is made.
private object ChatFilesFolder {
    private var cleared = false

    @Synchronized
    fun clearOnce(cache: File) {
        if (cleared) return
        File(cache, "chat-files").deleteRecursively()
        cleared = true
    }
}

private object UploadsFolder {
    private var cleared = false

    @Synchronized
    fun clearOnce(cache: File) {
        if (cleared) return
        File(cache, "uploads").deleteRecursively()
        cleared = true
    }
}
