package com.roversx.thinkterm

import android.content.Context
import androidx.compose.runtime.mutableStateMapOf

/// The open connections, one per host, outliving the terminal screen:
/// leaving the screen takes the picture away and nothing else, so a
/// host list visited in between, or another host opened, finds them
/// where they were. A host is closed from the list, or with the app.
object Sessions {
    private val models = mutableStateMapOf<String, TerminalModel>()

    fun get(context: Context, store: HostStore, host: Host): TerminalModel =
        models.getOrPut(host.id) { TerminalModel(context.applicationContext, store, host) }

    fun existing(hostId: String): TerminalModel? = models[hostId]

    fun isConnected(hostId: String): Boolean = models[hostId]?.isConnected == true

    fun close(hostId: String) {
        models.remove(hostId)?.shutdown()
    }

    val all: Collection<TerminalModel> get() = models.values

    /// The app left the screen or came back: every connection hears.
    fun enteredBackground() = all.forEach { it.enteredBackground() }
    fun enteredForeground() = all.forEach { it.enteredForeground() }
}
