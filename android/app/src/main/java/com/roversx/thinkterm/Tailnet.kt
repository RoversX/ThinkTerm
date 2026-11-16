package com.roversx.thinkterm

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.util.Log
import androidx.compose.foundation.background
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.width
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import com.roversx.thinkterm.core.qrPicture
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.graphics.FilterQuality
import androidx.compose.ui.draw.clip
import androidx.compose.ui.Alignment
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.layout.requiredSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.browser.customtabs.CustomTabsIntent
import com.roversx.thinkterm.core.TailnetConfig
import com.roversx.thinkterm.core.TailnetPeer
import com.roversx.thinkterm.core.TailnetStatus
import com.roversx.thinkterm.core.tailnetNewKeys
import com.roversx.thinkterm.core.tailnetRestart
import com.roversx.thinkterm.core.tailnetSet
import com.roversx.thinkterm.core.tailnetStatus
import com.roversx.thinkterm.core.tailnetSupported
import kotlinx.coroutines.delay

/// The app's own device in a tailnet: ThinkTerm joins by itself, so the
/// tailnet's machines are reached without the Tailscale app. What it runs
/// on is the user's (whether to join, a control server, a name) and the
/// device's (its keys, an auth key: in the encrypted preferences); the node
/// itself is the core's (`tailnetSet`), which this tells what to run and
/// asks how it goes. From ios/Sources/Tailnet.swift.
object TailnetStore {
    private const val KEYS = "tailnet.keys"
    private const val AUTH_KEY = "tailnet.authkey"

    private lateinit var prefs: android.content.SharedPreferences
    private lateinit var store: HostStore
    private lateinit var app: Context
    /// The login page is up, in the app: it goes once the device is in.
    private var loginOpen = false
    private val main = Handler(Looper.getMainLooper())

    /// Whether this build can join at all.
    val supported: Boolean by lazy { tailnetSupported() }

    var status by mutableStateOf(TailnetStatus(0uL, "off", "", "", "", "", emptyList()))
        private set
    /// Why the settings as they stand start no node.
    var settingsError by mutableStateOf("")
        private set
    var enabled by mutableStateOf(false)
        private set
    /// Blank for Tailscale's own.
    var controlUrl by mutableStateOf("")
        private set
    /// Blank for a name made from the device's keys.
    var hostname by mutableStateOf("")
        private set

    fun init(context: Context, hosts: HostStore) {
        if (::prefs.isInitialized) return
        app = context.applicationContext
        prefs = app.getSharedPreferences("thinkterm-settings", Context.MODE_PRIVATE)
        store = hosts
        enabled = prefs.getBoolean("tailnet.enabled", false)
        controlUrl = prefs.getString("tailnet.control", "") ?: ""
        hostname = prefs.getString("tailnet.hostname", "") ?: ""
        hasAuthKey = store.appSecret(AUTH_KEY) != null
    }

    fun join(on: Boolean) {
        enabled = on
        prefs.edit().putBoolean("tailnet.enabled", on).apply()
        apply()
    }

    fun keepControlUrl(text: String) {
        controlUrl = text
        prefs.edit().putString("tailnet.control", text).apply()
    }

    fun keepHostname(text: String) {
        hostname = text
        prefs.edit().putString("tailnet.hostname", text).apply()
    }

    /// An auth key, for joining without a login in the browser; kept with
    /// the other secrets, never shown again -- only whether there is one,
    /// which the page follows.
    var hasAuthKey by mutableStateOf(false)
        private set

    fun setAuthKey(key: String) {
        store.putAppSecret(AUTH_KEY, key.trim())
        hasAuthKey = store.appSecret(AUTH_KEY) != null
    }

    /// Has the core run a node on the settings as they stand, or none.
    fun apply() {
        if (!supported) return
        if (!enabled) {
            settingsError = tailnetSet(null)
            watch()
            return
        }
        // The device's identity in the tailnet: made once, kept until a
        // log out.
        val keys = store.appSecret(KEYS) ?: tailnetNewKeys()?.also { store.putAppSecret(KEYS, it) }
        if (keys == null) {
            settingsError = "no keys"
            return
        }
        settingsError = tailnetSet(TailnetConfig(keys, controlUrl, hostname, store.appSecret(AUTH_KEY) ?: ""))
        watch()
    }

    /// Try again, at a person's tap: the settings as they stand, and the
    /// node running made anew on them. Settings that did not change leave
    /// the node as it is, so `apply` alone would do nothing for one that
    /// failed.
    fun retry() {
        apply()
        tailnetRestart()
    }

    /// Leave the tailnet and forget the device: joining again is a new one.
    /// This device has an identity in a tailnet, signed in or about to be:
    /// there is something for Log Out to forget.
    val hasKeys: Boolean get() = store.appSecret(KEYS) != null

    fun logOut() {
        enabled = false
        prefs.edit().putBoolean("tailnet.enabled", false).apply()
        store.putAppSecret(KEYS, null)
        store.putAppSecret(AUTH_KEY, null)
        hasAuthKey = false
        loginOpen = false
        apply()
    }

    /// The status, asked every second while there is a node -- and while
    /// one is on its way out: a node stopped (a log out, the switch off)
    /// winds down after the call that stops it, still saying it is in, and
    /// its last word, off, must be read too or the page keeps the old one.
    private val poll = object : Runnable {
        override fun run() {
            refresh()
            if (enabled || status.state != "off") main.postDelayed(this, 1000)
        }
    }

    private fun watch() {
        main.removeCallbacks(poll)
        refresh()
        main.postDelayed(poll, 1000)
    }

    private fun refresh() {
        val now = tailnetStatus()
        if (now.revision != status.revision || now.state != status.state) status = now
        if (loginOpen && now.state == "connected") {
            loginOpen = false
            // The login page goes by itself once the device is in: the app
            // is brought back over it, which closes it.
            runCatching {
                app.startActivity(
                    Intent(app, MainActivity::class.java).addFlags(
                        Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP
                    )
                )
            }.onFailure { Log.w("thinkterm", "tailnet: could not come back over the login: $it") }
        }
    }

    /// The tailnet's login page, in the app: the system browser's own tab,
    /// which the sign-in providers accept where a web view of the app's
    /// they refuse.
    fun openLogin(context: Context, url: String) {
        loginOpen = true
        runCatching {
            CustomTabsIntent.Builder().setShowTitle(true).build().launchUrl(context, Uri.parse(url))
        }.onFailure {
            // No browser to host it: the plain way.
            runCatching { context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url))) }
        }
    }

    /// A debug build's `--ez tailnet_test true`: join, and log how the node
    /// stands as it changes -- the state alone, never the login link or a
    /// machine's name.
    fun debugJoin() {
        join(true)
        var last = ""
        main.post(object : Runnable {
            override fun run() {
                val s = status
                val now = "${s.state} link=${s.loginUrl.isNotEmpty()} machines=${s.peers.size} " +
                    "error=${s.error.isNotEmpty() || settingsError.isNotEmpty()}"
                if (now != last) {
                    last = now
                    Log.i("thinkterm", "TAILNETTEST $now")
                }
                main.postDelayed(this, 1000)
            }
        })
    }

    /// The machines of the tailnet other than this one, those that answer
    /// first and each group by name.
    val machines: List<TailnetPeer>
        get() = status.peers.sortedWith(
            compareByDescending<TailnetPeer> { it.online == true }.thenBy { it.name.lowercase() }
        )

    /// What the status says, in a line.
    val summary: String
        get() = when (status.state) {
            "starting" -> tr("tn.starting")
            "needs_login" -> tr("tn.needslogin")
            "needs_approval" -> tr("tn.needsapproval")
            "connected" -> tr("tn.connected", status.name)
            "failed" -> tr("tn.failed")
            else -> tr("tn.off")
        }

    val color: Color
        get() = when (status.state) {
            "connected" -> Color(0xFF34C759)
            "starting", "needs_login", "needs_approval" -> Color(0xFFFF9F0A)
            "failed" -> Color(0xFFFF453A)
            else -> Color(0xFF8E8E93)
        }
}

/// The tailnet on the hosts' page: where it stands, the way to its
/// settings, and once in, its machines -- a tap on one opens the page of
/// the host saved for it, or adds a new one with its name filled in.
@Composable
fun TailnetSection(store: HostStore, openSettings: () -> Unit, open: (Host) -> Unit, add: (Host) -> Unit) {
    if (!TailnetStore.supported) return
    val colors = groupedColors()
    Group("Tailscale") {
        GroupRow(onClick = openSettings) {
            HostRowContent("Tailscale", TailnetStore.summary) {
                DesktopIcon("globe", size = 20.dp, tint = LeadBlue)
            }
            RowChevron()
        }
        if (TailnetStore.status.state == "connected") {
            val machines = TailnetStore.machines
            if (machines.isEmpty()) {
                GroupRow { RowValue(tr("tn.none")) }
            }
            for (peer in machines) {
                val saved = savedHost(store, peer, machines)
                GroupRow(onClick = {
                    if (saved != null) {
                        // Kept already: its own page.
                        open(saved)
                    } else {
                        // Its name is its address: the node knows it.
                        add(Host(name = peer.name, hostname = hostnameFor(peer)))
                    }
                }) {
                    // A machine that is away is quieter, the whole row.
                    Row(
                        Modifier.weight(1f).alpha(if (peer.online == false) 0.5f else 1f),
                        verticalAlignment = androidx.compose.ui.Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                    ) {
                        HostRowContent(peer.name, machineDetail(peer)) {
                            val (mark, brand) = machineMark(peer)
                            if (brand) {
                                BrandIcon(mark, size = 19.dp, tint = colors.primary)
                            } else {
                                DesktopIcon(mark, size = 20.dp, tint = colors.primary)
                            }
                        }
                        if (saved != null) {
                            Text(tr("tn.added"), color = colors.secondary, fontSize = 14.sp)
                        } else {
                            Box(
                                Modifier.size(28.dp).background(LeadBlue.copy(alpha = 0.16f), CircleShape),
                                contentAlignment = androidx.compose.ui.Alignment.Center,
                            ) {
                                DesktopIcon("plus", size = 15.dp, tint = LeadBlue)
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Whether another of the tailnet's machines has this one's name: one of
/// its own and one shared into it from another tailnet, say. The node
/// takes that name for its own tailnet's machine; the full names tell the
/// two apart.
private fun nameShared(peer: TailnetPeer, peers: List<TailnetPeer>): Boolean =
    peers.count { it.name.equals(peer.name, ignoreCase = true) } > 1

/// What a host for this machine connects to: its full name. Its short name
/// may come to stand for another -- one of this tailnet's own that joins
/// under it later, which the node takes before a machine shared in.
private fun hostnameFor(peer: TailnetPeer): String =
    peer.fqdn.trimEnd('.').ifEmpty { peer.name }

/// The saved host that answers to this machine: by its full name or an
/// address, or by its name where no other machine has it too.
private fun savedHost(store: HostStore, peer: TailnetPeer, peers: List<TailnetPeer>): Host? {
    val name = if (nameShared(peer, peers)) "" else peer.name
    val names = listOf(name, peer.fqdn, peer.fqdn.trimEnd('.'), peer.ipv4, peer.ipv6)
        .filter { it.isNotEmpty() }.map { it.lowercase() }
    return store.hosts.firstOrNull { it.hostname.lowercase() in names }
}

/// "macOS · Online", as much of it as the machine says.
private fun machineDetail(peer: TailnetPeer): String {
    val state = peer.online?.let { tr(if (it) "tn.online" else "tn.offline") }
    return listOfNotNull(peer.os, state).filter { it.isNotEmpty() }.joinToString(" · ")
}

/// The maker's mark where there is one -- the distribution's, for Linux --
/// and a plain picture where not: the desktop's choice (`machine_icon` in
/// its hosts page). True with a mark's name, false with a desktop icon's.
private fun machineMark(peer: TailnetPeer): Pair<String, Boolean> = when (peer.os.lowercase()) {
    "macos", "ios", "ipados", "tvos" -> "apple" to true
    "android" -> "android" to true
    "linux" -> (distroMark(peer.distro) ?: "linux") to true
    // The app carries no Windows mark.
    "windows" -> "monitor" to false
    else -> "server" to false
}

/// A distribution's mark, by the name it gives itself (the desktop's
/// `distro_to_icon`).
private fun distroMark(id: String): String? = when (id.trim().lowercase()) {
    "ubuntu" -> "ubuntu"
    "debian" -> "debian"
    "raspbian" -> "raspberrypi"
    "fedora" -> "fedora"
    "arch", "archlinux", "arch-linux" -> "archlinux"
    "alpine" -> "alpinelinux"
    "rhel", "redhat", "red hat" -> "redhat"
    "centos" -> "centos"
    "nixos" -> "nixos"
    "proxmox", "pve" -> "proxmox"
    else -> null
}

/// Whether ThinkTerm joins a tailnet: where it stands and the one thing to
/// do about it first, then what this device is in it; its own settings (a
/// company's or one's own control server) on a page of their own.
@Composable
fun TailnetSettingsScreen(onBack: () -> Unit) {
    var advanced by remember { mutableStateOf(false) }
    BackHandler(enabled = advanced) { advanced = false }
    AnimatedContent(
        targetState = advanced,
        transitionSpec = { pushTransition(targetState) },
        label = "tailnet-advanced",
    ) { open ->
        if (open) TailnetAdvancedScreen { advanced = false } else TailnetMainScreen(onBack) { advanced = true }
    }
}

@Composable
private fun TailnetMainScreen(onBack: () -> Unit, openAdvanced: () -> Unit) {
    val context = LocalContext.current
    val colors = groupedColors()
    val status = TailnetStore.status
    // Where it stands, off when not asked for whatever the node said last.
    val state = if (TailnetStore.enabled) status.state else "off"
    var copied by remember { mutableStateOf(false) }
    var copiedAddress by remember { mutableStateOf(false) }
    LaunchedEffect(copied, copiedAddress) {
        if (copied || copiedAddress) {
            delay(1200)
            copied = false
            copiedAddress = false
        }
    }
    fun copy(text: String) {
        val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        clipboard.setPrimaryClip(ClipData.newPlainText("Tailscale", text))
    }

    GroupedPage("", onBack = onBack) {
        // Its tile, where it stands, a word on that, and the way on.
        Column(
            Modifier.fillMaxWidth().padding(start = 24.dp, end = 24.dp, bottom = 14.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Box {
                GlassTile("globe", TileColors.blue, 76.dp)
                if (state != "off") {
                    Box(
                        Modifier
                            .align(Alignment.BottomEnd)
                            .offset(4.dp, 4.dp)
                            .size(16.dp)
                            .background(colors.ground, CircleShape)
                            .padding(3.dp)
                            .background(TailnetStore.color, CircleShape),
                    )
                }
            }
            Spacer(Modifier.height(14.dp))
            Text("Tailscale", color = colors.primary, fontSize = 28.sp, fontWeight = FontWeight.Bold)
            Spacer(Modifier.height(10.dp))
            val pill = if (state == "off") colors.secondary else TailnetStore.color
            Row(
                Modifier.clip(CircleShape).background(pill.copy(alpha = 0.14f)).padding(horizontal = 11.dp, vertical = 5.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Box(Modifier.size(8.dp).background(pill, CircleShape))
                Spacer(Modifier.width(6.dp))
                Text(if (state == "off") tr("tn.off") else TailnetStore.summary, color = pill, fontSize = 13.sp, fontWeight = FontWeight.Medium)
            }
            val blurb = when (state) {
                "off" -> tr("tn.join.foot")
                "needs_login" -> tr("tn.login.body")
                // Not an error: something for a person to do elsewhere.
                "needs_approval" -> tr("tn.approval.hint")
                "connected" -> tr("tn.connected.body")
                else -> ""
            }
            if (blurb.isNotEmpty()) {
                Spacer(Modifier.height(10.dp))
                Text(blurb, color = colors.secondary, fontSize = 14.sp, textAlign = TextAlign.Center)
            }
            // The settings' own error even while it is off: they are why it
            // did not start. While it is connecting, what held up its last
            // try, quietly: it tries again by itself, nothing for a person
            // to do.
            val error = TailnetStore.settingsError.ifEmpty { if (state == "off") "" else status.error }
            val quiet = TailnetStore.settingsError.isEmpty() && state == "starting"
            if (error.isNotEmpty()) {
                Spacer(Modifier.height(8.dp))
                Text(error, color = if (quiet) colors.secondary else colors.destructive, fontSize = 13.sp, textAlign = TextAlign.Center)
            }
            when (state) {
                "off" -> WideButton(tr("tn.signin"), "plus", lead = true) { TailnetStore.join(true) }
                "failed" -> WideButton(tr("retry"), "rotate-ccw", lead = true) { TailnetStore.retry() }
                "needs_login" -> Row(Modifier.fillMaxWidth().padding(top = 20.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    if (status.loginUrl.isNotEmpty()) {
                        WideButton(tr("tn.login"), "globe", lead = true, top = 0.dp, modifier = Modifier.weight(1f)) {
                            TailnetStore.openLogin(context, status.loginUrl)
                        }
                    }
                    WideButton(tr(if (copied) "copied" else "copy"), if (copied) "check" else "copy", lead = false, top = 0.dp, modifier = Modifier.weight(1f)) {
                        copy(status.loginUrl)
                        copied = true
                    }
                }
                else -> {}
            }
        }

        if (state == "needs_login" && status.loginUrl.isNotEmpty()) {
            Group(tr("tn.qr.title")) { LoginCode(status.loginUrl) }
        }

        if (state == "connected") {
            Group(tr("tn.thisdevice")) {
                GroupRow {
                    RowTitle(tr("f.name"))
                    RowValue(status.name)
                }
                if (status.address.isNotEmpty()) {
                    GroupRow(onClick = {
                        copy(status.address)
                        copiedAddress = true
                    }) {
                        RowTitle(tr("tn.address"))
                        Text(status.address, color = colors.primary, fontSize = 15.sp, fontFamily = FontFamily.Monospace)
                        DesktopIcon(if (copiedAddress) "check" else "copy", size = 17.dp, tint = LeadBlue)
                    }
                }
                GroupRow {
                    RowTitle(tr("tn.machines"))
                    RowValue(tr("tn.online.count", TailnetStore.machines.count { it.online == true }.toString()))
                }
            }
        }

        Group {
            GroupRow(onClick = openAdvanced) {
                RowTitle(tr("f.advanced"))
                RowChevron()
            }
        }

        // Ending it: for now, or for good.
        if (TailnetStore.enabled) {
            Group(footer = tr("tn.turnoff.foot")) {
                GroupRow(onClick = { TailnetStore.join(false) }) { RowTitle(tr("tn.turnoff"), color = LeadBlue) }
                GroupRow(onClick = { TailnetStore.logOut() }) { RowTitle(tr("tn.logout"), color = colors.destructive) }
            }
        } else if (TailnetStore.hasKeys) {
            Group {
                GroupRow(onClick = { TailnetStore.logOut() }) { RowTitle(tr("tn.logout"), color = colors.destructive) }
            }
        }
    }
}

/// A button as wide as there is room for: the accent's for the one that
/// leads, glass for the other.
@Composable
private fun WideButton(
    label: String,
    icon: String,
    lead: Boolean,
    top: androidx.compose.ui.unit.Dp = 20.dp,
    modifier: Modifier = Modifier.fillMaxWidth(),
    onClick: () -> Unit,
) {
    val colors = groupedColors()
    val ink = if (lead) Color.White else colors.primary
    Row(
        modifier
            .padding(top = top)
            .height(50.dp)
            .clip(CircleShape)
            .then(if (lead) Modifier.background(LeadBlue) else Modifier.glass(LocalGroundGlass.current, colors.dark, CircleShape))
            .clickable(onClick = onClick)
            // Clear of the capsule's round ends, two side by side too.
            .padding(horizontal = 16.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
    ) {
        DesktopIcon(icon, size = 18.dp, tint = ink)
        Spacer(Modifier.width(7.dp))
        Text(
            label, color = ink, fontSize = 16.sp, fontWeight = FontWeight.SemiBold,
            maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false),
        )
    }
}

/// The tailnet's own settings, for a company's or one's own control server:
/// kept as they are left, and the node started on them then.
@Composable
private fun TailnetAdvancedScreen(onBack: () -> Unit) {
    var authKey by remember { mutableStateOf("") }
    var control by remember { mutableStateOf(TailnetStore.controlUrl) }
    var name by remember { mutableStateOf(TailnetStore.hostname) }
    // What was typed and left is kept, as a field's return keeps it.
    DisposableEffect(Unit) {
        onDispose {
            val changed = control != TailnetStore.controlUrl || name != TailnetStore.hostname || authKey.isNotEmpty()
            TailnetStore.keepControlUrl(control)
            TailnetStore.keepHostname(name)
            if (authKey.isNotEmpty()) TailnetStore.setAuthKey(authKey)
            if (changed && TailnetStore.enabled) TailnetStore.apply()
        }
    }
    GroupedPage(tr("f.advanced"), onBack = onBack) {
        Group {
            FieldRow(
                tr("tn.authkey"),
                authKey,
                { authKey = it },
                placeholder = if (TailnetStore.hasAuthKey) "••••••••" else "",
                keyboard = KeyboardType.Password,
                secret = true,
            )
            FieldRow(tr("tn.control"), control, { control = it }, placeholder = "controlplane.tailscale.com", keyboard = KeyboardType.Uri)
            FieldRow(tr("tn.name"), name, { name = it }, placeholder = tr("tn.auto"))
        }
    }
}

/// The login link as a code another device's camera reads, to log this one
/// in there (a computer's password manager, a security key). The core draws
/// it (`qrPicture`), as it draws it for the iOS app; shown pixel for pixel,
/// dark on white whatever the appearance.
@Composable
private fun LoginCode(link: String) {
    val colors = groupedColors()
    val density = LocalDensity.current
    val pixels = with(density) { 200.dp.roundToPx() }
    val code = remember(link, pixels) {
        qrPicture(link, pixels.toUInt())?.let { CorePictures.bitmap(it.rgba, it.width.toInt(), it.height.toInt()) }
    }
    Column(
        Modifier.fillMaxWidth().background(colors.card).padding(16.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        if (code != null) {
            Image(
                code,
                contentDescription = null,
                filterQuality = FilterQuality.None,
                modifier = Modifier
                    .requiredSize(with(density) { code.width.toDp() })
                    .clip(RoundedCornerShape(12.dp)),
            )
        }
        Text(tr("tn.qr"), color = colors.secondary, fontSize = 13.sp, textAlign = TextAlign.Center)
    }
}
