package com.roversx.thinkterm

import android.content.pm.ApplicationInfo
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Dns
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // The core opens the faces as files, so they leave the APK once.
        Assets.install(this)
        AppSettings.get(this)
        L10n.load(this)
        Schemes.load(this)
        AppLanguage.tag = AppSettings.get(this).language
        val debuggable = (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE) != 0
        val store = HostStore(this)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Root(store, debuggable)
            }
        }
    }
}

/// The two tabs. An open host takes the whole screen, the tab bar
/// included, the way the terminal does on iOS.
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun Root(store: HostStore, debuggable: Boolean) {
    // Reading the tag here relabels the tabs with the rest of the app.
    AppLanguage.tag
    var open by remember { mutableStateOf<Host?>(null) }
    var tab by remember { mutableIntStateOf(0) }

    val host = open
    if (host != null) {
        TerminalScreen(host, store) { open = null }
        return
    }

    Scaffold(
        modifier = Modifier.fillMaxSize(),
        bottomBar = {
            NavigationBar {
                NavigationBarItem(
                    selected = tab == 0,
                    onClick = { tab = 0 },
                    icon = { Icon(Icons.Default.Dns, null) },
                    label = { Text(tr("hosts")) },
                )
                NavigationBarItem(
                    selected = tab == 1,
                    onClick = { tab = 1 },
                    icon = { Icon(Icons.Default.Settings, null) },
                    label = { Text(tr("settings")) },
                )
            }
        },
    ) { padding ->
        // The bar over them spends the insets; the tabs must not again.
        Box(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .consumeWindowInsets(padding)
        ) {
            if (tab == 0) {
                HostsScreen(store, showProbe = debuggable) { open = it }
            } else {
                SettingsScreen(model = null, showLog = null, onDone = null)
            }
        }
    }
}
