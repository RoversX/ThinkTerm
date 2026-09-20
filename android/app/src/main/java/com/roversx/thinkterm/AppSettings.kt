package com.roversx.thinkterm

import android.content.Context
import android.content.SharedPreferences
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import kotlin.properties.ReadWriteProperty
import kotlin.reflect.KProperty

/// The app's preferences, one object for every screen: the Settings tab
/// edits them with no terminal open, and an open terminal follows them
/// as they change. The keys and values are the iOS app's
/// (ios/Sources/AppSettings.swift), so the two stay one design.
class AppSettings private constructor(private val prefs: SharedPreferences) {
    companion object {
        @Volatile private var instance: AppSettings? = null

        fun get(context: Context): AppSettings = instance ?: synchronized(this) {
            instance ?: AppSettings(
                context.applicationContext.getSharedPreferences("thinkterm-settings", Context.MODE_PRIVATE)
            ).also { instance = it }
        }

        /// Only after `get` has run once (MainActivity does), for code
        /// with no Context at hand.
        val shared: AppSettings get() = instance ?: error("AppSettings.get(context) first")
    }

    // MARK: general

    /// A tag of strings.json, or "system" to follow the phone's.
    var language by pref("lang", "system") { AppLanguage.tag = it }

    // MARK: appearance

    /// A scheme name from schemes.json, or "desktop" to follow the host's.
    var schemeName by pref("scheme.name", Schemes.FOLLOW_DESKTOP)
    /// The text size a connection starts at, in points.
    var fontSize by pref("font.size", 11.0)
    /// One of the bundled faces; a change reconnects to shape with it.
    var fontFamily by pref("font.family", "JetBrains Mono")
    /// "off", "3", "45" or "7": a WCAG floor text is lifted to.
    var contrast by pref("text.contrast", "off")

    // MARK: terminal

    /// Smooth (by the pixel, with inertia) or stepped (whole rows).
    var smoothScroll by pref("scroll.smooth", true)
    /// A thin bar at the right while the scrollback moves.
    var scrollbar by pref("scroll.bar", false)
    /// "auto" (the program's), "block", "bar" or "underline".
    var cursorStyle by pref("cursor.style", "block")
    var cursorBlink by pref("cursor.blink", true)
    /// A pane's bell buzzes the phone.
    var bell by pref("bell", false)
    /// The terminal's keyboard is asked not to learn from what is typed
    /// (Gboard shows its incognito look). Password fields always are.
    var incognitoKeyboard by pref("keyboard.incognito", true)
    /// "auto" or "live": the server's panes follow a divider drag as it
    /// goes; "release": once, when the finger lifts.
    var resizeMode by pref("resize.mode", "auto")

    // MARK: interface

    /// "one" or "two" levels of tabs: threads over tabs, or tabs alone.
    var tabBarLevels by pref("ui.tabbar", "two")
    /// The bars over split panes; off, the rows go to the terminal.
    var paneBars by pref("ui.panebars", true)

    // MARK: keyboard

    var hapticKeys by pref("key.haptics", true)
    /// The key panel opens with the software keyboard.
    var autoKeyPanel by pref("key.autopanel", false)

    // MARK: gestures

    var pinchZoom by pref("gesture.pinch", true)
    var twoFingerHidesKeyboard by pref("gesture.twofinger", true)
    var longPressSelects by pref("gesture.longpress", true)

    // MARK: connection

    var autoReconnect by pref("conn.autoreconnect", true)
    /// Seconds between ssh keep-alives, 0 for none; the next connection
    /// takes it.
    var keepAliveSeconds by pref("conn.keepalive", 30)
    /// Hold the connection when the app goes to the background; off,
    /// drop it at once and redial on return.
    var keepSessionInBackground by pref("conn.background", true)

    // MARK: diagnostics

    var devMode by pref("dev.mode", false)

    /// A preference as Compose state: reads subscribe, writes persist.
    private inline fun <reified T> pref(key: String, default: T, crossinline changed: (T) -> Unit = {}) =
        object : ReadWriteProperty<AppSettings, T> {
            private var state by mutableStateOf(load(key, default))

            override fun getValue(thisRef: AppSettings, property: KProperty<*>): T = state

            override fun setValue(thisRef: AppSettings, property: KProperty<*>, value: T) {
                if (state == value) return
                state = value
                store(key, value)
                changed(value)
            }
        }

    @Suppress("UNCHECKED_CAST")
    private fun <T> load(key: String, default: T): T = when (default) {
        is String -> (prefs.getString(key, default) ?: default) as T
        is Boolean -> prefs.getBoolean(key, default) as T
        is Int -> prefs.getInt(key, default) as T
        is Double -> (if (prefs.contains(key)) prefs.getFloat(key, default.toFloat()).toDouble() else default) as T
        else -> default
    }

    private fun <T> store(key: String, value: T) {
        val e = prefs.edit()
        when (value) {
            is String -> e.putString(key, value)
            is Boolean -> e.putBoolean(key, value)
            is Int -> e.putInt(key, value)
            is Double -> e.putFloat(key, value.toFloat())
        }
        e.apply()
    }
}
