package com.roversx.thinkterm

import android.content.pm.ApplicationInfo
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // The core opens the faces as files, so they leave the APK once.
        Assets.install(this)
        val debuggable = (applicationInfo.flags and ApplicationInfo.FLAG_DEBUGGABLE) != 0
        val store = HostStore(this)
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                var open by remember { mutableStateOf<Host?>(null) }
                val host = open
                if (host == null) {
                    HostsScreen(store, showProbe = debuggable) { open = it }
                } else {
                    TerminalScreen(host, store) { open = null }
                }
            }
        }
    }
}
