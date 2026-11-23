package com.roversx.thinkterm

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import org.json.JSONObject

/// Where a notification's tap goes: the agent it was about, in its host's
/// terminal, its chat open over it when it has one.
data class AgentLink(val hostId: String, val pane: Int, val agent: String, val place: String, val chat: Boolean)

/// A tap on an agent's notification, waiting for its host's terminal: the
/// app goes to that host (`Root`), and once its terminal is attached the
/// agent is shown (`TerminalScreen`). Main thread only.
object AgentLinks {
    const val ACTION = "com.roversx.thinkterm.OPEN_AGENT"

    var pending by mutableStateOf<AgentLink?>(null)

    /// The link `intent` carries, if it is a notification's.
    fun take(intent: Intent?) {
        if (intent?.action != ACTION) return
        val host = intent.getStringExtra("host") ?: return
        pending = AgentLink(
            hostId = host,
            pane = intent.getIntExtra("pane", -1).takeIf { it >= 0 } ?: return,
            agent = intent.getStringExtra("agent").orEmpty(),
            place = intent.getStringExtra("place").orEmpty(),
            chat = intent.getBooleanExtra("chat", false),
        )
    }
}

/// A notification when an agent begins to wait on the person -- an
/// approval, a question -- while the app is away: the connection kept in
/// the background (`ConnectionService`) hears it, the core says so
/// (`onPublished`'s "agent_waiting"), and it goes again when the agent
/// moves on ("agent_resumed"). On screen the Agents list says it already.
object AgentAlerts {
    private const val CHANNEL = "agents"
    private const val ID = 7501

    fun waiting(context: Context, host: Host, json: String) {
        if (!Sessions.away) return
        val told = runCatching { JSONObject(json) }.getOrNull() ?: return
        val notifications = NotificationManagerCompat.from(context)
        if (!notifications.areNotificationsEnabled()) return
        val manager = context.getSystemService(NotificationManager::class.java) ?: return
        if (manager.getNotificationChannel(CHANNEL) == null) {
            manager.createNotificationChannel(
                NotificationChannel(CHANNEL, tr("chat.notify.channel"), NotificationManager.IMPORTANCE_HIGH)
            )
        }
        val pane = told.optInt("pane")
        val place = told.optString("place")
        val detail = told.optString("detail")
        val text = listOf(place, detail).filter { it.isNotEmpty() }.joinToString(" · ")
        // A tap goes to the agent: its host's terminal, its chat over it
        // when it has one. The screen there takes it in (`onNewIntent`).
        val launch = Intent(context, MainActivity::class.java)
            .setAction(AgentLinks.ACTION)
            .putExtra("host", host.id)
            .putExtra("pane", pane)
            .putExtra("agent", told.optString("agent"))
            .putExtra("place", place)
            .putExtra("chat", told.optBoolean("chat"))
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        // One request code a notification, so each keeps its own agent.
        val open = PendingIntent.getActivity(
            context,
            tag(host, pane).hashCode(),
            launch,
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val notification = NotificationCompat.Builder(context, CHANNEL)
            .setSmallIcon(R.drawable.ic_notify_terminal)
            .setContentTitle(tr("chat.notify.title", told.optString("name")))
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setSubText(host.name.ifEmpty { host.hostname })
            .setContentIntent(open)
            .setAutoCancel(true)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .build()
        runCatching { manager.notify(tag(host, pane), ID, notification) }
    }

    fun resumed(context: Context, host: Host, pane: String) {
        val id = pane.toIntOrNull() ?: return
        context.getSystemService(NotificationManager::class.java)?.cancel(tag(host, id), ID)
    }

    private fun tag(host: Host, pane: Int) = "agent:${host.id}:$pane"
}
