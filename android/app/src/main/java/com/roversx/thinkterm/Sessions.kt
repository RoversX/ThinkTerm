package com.roversx.thinkterm

import android.annotation.SuppressLint
import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import androidx.compose.runtime.mutableStateMapOf
import com.roversx.thinkterm.core.tailnetRestart

/// The open connections, one per host, outliving the terminal screen:
/// leaving the screen takes the picture away and nothing else, so a
/// host list visited in between, or another host opened, finds them
/// where they were. A host is closed from the list, or with the app.
object Sessions {
    private val models = mutableStateMapOf<String, TerminalModel>()

    fun get(context: Context, store: HostStore, host: Host): TerminalModel =
        models.getOrPut(host.id) {
            // As the store has it now: a page's copy may be from before its
            // key was forgotten there, and would dial with the old one.
            TerminalModel(context.applicationContext, store, store.hosts.firstOrNull { it.id == host.id } ?: host)
        }

    fun existing(hostId: String): TerminalModel? = models[hostId]

    fun isConnected(hostId: String): Boolean = models[hostId]?.isConnected == true

    fun close(hostId: String) {
        models.remove(hostId)?.shutdown()
        connectionsChanged()
    }

    val all: Collection<TerminalModel> get() = models.values

    /// The app's own context, once it has started (`watchNetwork`): the
    /// application's, which lives as long as the process does.
    @SuppressLint("StaticFieldLeak")
    private var app: Context? = null

    /// A connection came up, went or is on its way back: the service that
    /// keeps them up while the app is out of sight goes on for as long as
    /// one is, with the setting on (`ConnectionService`).
    fun connectionsChanged() {
        val app = app ?: return
        val kept = if (!AppSettings.get(app).keepSessionInBackground) emptyList()
        else all.filter { it.keepsUp }.map { it.host.title }.sorted()
        ConnectionService.keep(app, kept)
    }

    /// When the app last left the screen, on the clock that goes on while
    /// the phone sleeps (the uptime clock stops in deep sleep, and a phone
    /// put away for an hour would seem to have been gone for seconds).
    private var leftAt: Long? = null
    /// Away this long, the built-in tailnet's node is made anew on the way
    /// back: what it held -- its way to the control server and its relays
    /// -- went stale while the app was frozen, and every connection through
    /// it would wait out a timeout before finding so.
    private const val TAILNET_STALE_AFTER_MS = 120_000L

    /// The app is off the screen: what waits on the person is told by a
    /// notification (`AgentAlerts`).
    val away: Boolean get() = leftAt != null

    /// The app left the screen or came back: every connection hears.
    fun enteredBackground() {
        leftAt = SystemClock.elapsedRealtime()
        all.forEach { it.enteredBackground() }
    }

    fun enteredForeground() {
        val left = leftAt
        // Back from a picker of the app's own, the node stays: the file
        // picked goes up on the connection it carries, and a picker kept
        // open long is no frozen app (`TerminalModel.picking`).
        val away = left != null && SystemClock.elapsedRealtime() - left >= TAILNET_STALE_AFTER_MS
        if (away && all.none { it.picking }) restartTailnet()
        leftAt = null
        all.forEach { it.enteredForeground() }
        // A connection that came up while the app was away could not start
        // the service then (the system allows it only from the screen).
        connectionsChanged()
    }

    /// When the node was last made anew from here, on `elapsedRealtime`.
    private var restartedAt: Long? = null

    private fun restartTailnet() {
        restartedAt = SystemClock.elapsedRealtime()
        tailnetRestart()
    }

    // MARK: the network

    private var watching = false
    /// The default network last told of, or none while there is none.
    private var network: Network? = null
    private var networkSeen = false
    private val main = Handler(Looper.getMainLooper())

    /// How long a new network is let settle before the node is made anew
    /// on it: one that comes and goes, or hands over and back, costs one
    /// restart -- and every tailnet session it carries -- not one each.
    private const val NETWORK_SETTLE_MS = 2_000L

    /// When the change `settled` waits on was heard of, on `elapsedRealtime`.
    private var changedAt = 0L

    /// A restart this close before a change was heard of was made on the
    /// new network already: the return from a long absence, which hears of
    /// the network it came back to at the same moment.
    private const val RESTART_COVERS_MS = 1_000L

    /// The tailnet's node is made anew on the network now in use -- Android
    /// tells it nothing of one itself -- unless that was just done, when
    /// another would only cut the connections the first is redialing; and
    /// every connection is looked at now, not when its keep-alives give up.
    private val settled = Runnable {
        val restarted = restartedAt
        if (restarted == null || restarted < changedAt - RESTART_COVERS_MS) restartTailnet()
        all.forEach { it.checkConnection() }
    }

    /// Hear from the system when the default network changes: another one,
    /// or one back after none. Once per process.
    fun watchNetwork(context: Context) {
        app = context.applicationContext
        if (watching) return
        val connectivity = context.applicationContext.getSystemService(ConnectivityManager::class.java) ?: return
        watching = true
        // With a network now, the system tells of it at once, and that word
        // is no change. With none, the first that comes is one back.
        if (connectivity.activeNetwork == null) networkSeen = true
        connectivity.registerDefaultNetworkCallback(object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                main.post { networkIs(network) }
            }

            override fun onLost(network: Network) {
                main.post { if (this@Sessions.network == network) networkIs(null) }
            }
        })
    }

    private fun networkIs(next: Network?) {
        val seen = networkSeen
        val before = network
        network = next
        networkSeen = true
        if (!seen || next == before) return
        // Another change before the last one settled starts the wait again;
        // with no network at all there is nothing to make anew on.
        main.removeCallbacks(settled)
        if (next != null) {
            changedAt = SystemClock.elapsedRealtime()
            main.postDelayed(settled, NETWORK_SETTLE_MS)
        }
    }
}
