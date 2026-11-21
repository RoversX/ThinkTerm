package com.roversx.thinkterm

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat

/// Keeps the app's connections up while it is out of sight: a foreground
/// service, and the notification that goes with one, for as long as a
/// session is connected, on its way in or on its way back, and the setting
/// to keep them in the background is on. Without it Android freezes the
/// app soon after it leaves the screen, and every connection with it.
///
/// It is started while the app is on screen (`Sessions.connectionsChanged`),
/// which the system allows where a start from the background it does not;
/// swiping the app away ends it (`stopWithTask`), as it ends the app. Its
/// notification shows after the system's usual few seconds, so a
/// connection that fails at once puts none up.
class ConnectionService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE
        } else 0
        // Once asked to start, it must go to the foreground, even when the
        // connections it was for are gone by now: the system ends an app
        // whose service it was asked for and that did not.
        try {
            ServiceCompat.startForeground(this, NOTIFICATION_ID, notification(this, shown), type)
        } catch (e: Exception) {
            Log.w("thinkterm", "keeping the connections: $e")
            state = State.STOPPED
            stopSelf()
            return START_NOT_STICKY
        }
        if (shown.isEmpty()) {
            ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
            stopSelf()
            state = State.STOPPED
        } else {
            state = State.RUNNING
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        state = State.STOPPED
        super.onDestroy()
    }

    private enum class State { STOPPED, STARTING, RUNNING }

    companion object {
        private const val CHANNEL = "connections"
        private const val NOTIFICATION_ID = 1

        // Read and written on the main thread only, as `keep` and the
        // service's own calls all are.
        private var state = State.STOPPED
        /// The hosts the notification names; none, and it goes.
        private var shown: List<String> = emptyList()

        /// Keep the connections to `hosts` up, or -- with none -- stop.
        fun keep(context: Context, hosts: List<String>) {
            val app = context.applicationContext
            val before = shown
            shown = hosts
            when (state) {
                State.STOPPED -> if (hosts.isNotEmpty()) {
                    try {
                        app.startForegroundService(Intent(app, ConnectionService::class.java))
                        state = State.STARTING
                    } catch (e: Exception) {
                        // Asked from the background (a redial that came up
                        // there): the next change on screen starts it.
                        Log.w("thinkterm", "not keeping the connections: $e")
                    }
                }
                // It takes `shown` as it is when it gets going.
                State.STARTING -> {}
                State.RUNNING -> when {
                    hosts.isEmpty() -> {
                        app.stopService(Intent(app, ConnectionService::class.java))
                        state = State.STOPPED
                    }
                    hosts != before -> app.getSystemService(NotificationManager::class.java)
                        ?.notify(NOTIFICATION_ID, notification(app, hosts))
                }
            }
        }

        private fun notification(context: Context, hosts: List<String>): Notification {
            val manager = context.getSystemService(NotificationManager::class.java)
            if (manager != null && manager.getNotificationChannel(CHANNEL) == null) {
                manager.createNotificationChannel(
                    NotificationChannel(CHANNEL, tr("conn.notify.channel"), NotificationManager.IMPORTANCE_LOW).apply {
                        setShowBadge(false)
                    }
                )
            }
            // As the launcher opens it: the app as it was left.
            val launch = Intent(context, MainActivity::class.java)
                .setAction(Intent.ACTION_MAIN)
                .addCategory(Intent.CATEGORY_LAUNCHER)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED)
            val open = PendingIntent.getActivity(context, 0, launch, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            return NotificationCompat.Builder(context, CHANNEL)
                .setSmallIcon(R.drawable.ic_notify_terminal)
                .setContentTitle(tr("conn.notify.title"))
                .setContentText(hosts.joinToString(", "))
                .setContentIntent(open)
                .setOngoing(true)
                .setSilent(true)
                .setShowWhen(false)
                .setCategory(NotificationCompat.CATEGORY_SERVICE)
                .build()
        }
    }
}
