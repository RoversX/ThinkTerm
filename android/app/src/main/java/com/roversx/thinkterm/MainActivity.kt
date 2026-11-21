package com.roversx.thinkterm

import android.content.pm.ApplicationInfo
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.animation.AnimatedContent
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        // 15 and later lay every screen out under the system's bars by
        // themselves; 13 and 14 are asked to, so the app looks the same.
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.VANILLA_ICE_CREAM) enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        // The core opens the faces as files, so they leave the APK once.
        Assets.install(this)
        AppSettings.get(this)
        L10n.load(this)
        Schemes.load(this)
        AppLanguage.tag = AppSettings.get(this).language
        val debuggable = (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE) != 0
        GlassProbe.on = debuggable && intent.getBooleanExtra("glass_probe", false)
        val store = HostStore.get(this)
        // The tailnet ThinkTerm joins by itself is up before any host of it
        // is dialled.
        TailnetStore.init(this, store)
        Sessions.watchNetwork(this)
        if (debuggable && intent.getBooleanExtra("tailnet_test", false)) TailnetStore.debugJoin() else TailnetStore.apply()
        // A fresh start opens on the starry sky; a screen made again does
        // not. A first run with no host kept has its pages on it, and so
        // does a debug start asked to (`replay_onboarding`), hosts or not.
        val sky = savedInstanceState == null && !LaunchSky.played
        val settings = AppSettings.get(this)
        val replay = debuggable && intent.getBooleanExtra("replay_onboarding", false)
        val firstRun = sky && (replay || !settings.onboarded && store.hosts.isEmpty())
        setContent {
            // The app follows the system's light or dark; the terminal's
            // screen follows its scheme instead (TerminalChrome).
            MaterialTheme(colorScheme = appColors(isSystemInDarkTheme()), shapes = appShapes) {
                LaunchSkyHost(
                    play = sky,
                    onboard = firstRun,
                    pages = { enter, travel -> Onboarding(store, travel) { settings.onboarded = true; enter() } },
                ) { Root(store, debuggable) }
            }
        }
    }
}

/// The threads of every host, and over them a host's terminal, the hosts
/// or the settings, as iOS's navigation stack pushes them.
@Composable
private fun Root(store: HostStore, debuggable: Boolean) {
    // Reading the tag here relabels the screens with the rest of the app.
    AppLanguage.tag
    // Leaving the app (Home, another app) with the setting off drops
    // every connection; coming back redials them.
    val lifecycle = androidx.compose.ui.platform.LocalLifecycleOwner.current.lifecycle
    androidx.compose.runtime.DisposableEffect(lifecycle) {
        val observer = androidx.lifecycle.LifecycleEventObserver { _, event ->
            when (event) {
                androidx.lifecycle.Lifecycle.Event.ON_STOP -> Sessions.enteredBackground()
                androidx.lifecycle.Lifecycle.Event.ON_START -> Sessions.enteredForeground()
                else -> {}
            }
        }
        lifecycle.addObserver(observer)
        onDispose { lifecycle.removeObserver(observer) }
    }
    var page by remember { mutableStateOf(Page.THREADS) }
    var open by remember { mutableStateOf<Host?>(null) }
    // The host whose own page is up on the hosts' screen: kept here, so the
    // way back from its terminal comes back to it, as iOS's stack does.
    var hostPage by remember { mutableStateOf<String?>(null) }
    val route = open?.let { Route.OnTerminal(it) } ?: Route.OnPage(page)

    // A page pushed slides in over the one it leaves, as iOS's stack does.
    AnimatedContent(
        targetState = route,
        transitionSpec = { pushTransition(targetState.depth > initialState.depth) },
        contentKey = { it.key },
        label = "pages",
    ) { shown ->
        when (shown) {
            is Route.OnTerminal -> TerminalScreen(shown.host, store) { open = null }
            is Route.OnPage -> {
                SystemBars(dark = isSystemInDarkTheme())
                when (shown.page) {
                    Page.THREADS -> ThreadsHome(
                        store = store,
                        debuggable = debuggable,
                        onOpen = { open = it },
                        onHosts = { page = Page.HOSTS },
                        onSettings = { page = Page.SETTINGS },
                    )
                    Page.HOSTS -> {
                        BackHandler { page = Page.THREADS; hostPage = null }
                        HostsScreen(
                            store,
                            showProbe = false,
                            onBack = { page = Page.THREADS; hostPage = null },
                            hostPage = hostPage,
                            onHostPage = { hostPage = it },
                        ) { open = it }
                    }
                    Page.SETTINGS -> {
                        BackHandler { page = Page.THREADS }
                        Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
                            SettingsScreen(model = null, showLog = null, onDone = { page = Page.THREADS })
                        }
                    }
                }
            }
        }
    }
}

/// Where the app is: a page of its own, or a host's terminal over it.
/// Equal by value, so a recomposition is not a move; `key` says which
/// screen it is, so a host edited in place is not one either.
private sealed class Route {
    abstract val depth: Int
    abstract val key: String

    data class OnPage(val page: Page) : Route() {
        override val depth get() = if (page == Page.THREADS) 0 else 1
        override val key get() = page.name
    }

    data class OnTerminal(val host: Host) : Route() {
        override val depth get() = 2
        override val key get() = "terminal:" + host.id
    }
}

internal enum class Page { THREADS, HOSTS, SETTINGS }
