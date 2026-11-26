package com.roversx.thinkterm.applock

import android.app.Activity
import android.app.KeyguardManager
import android.content.Context
import android.content.SharedPreferences
import android.hardware.biometrics.BiometricManager
import android.hardware.biometrics.BiometricManager.Authenticators
import android.hardware.biometrics.BiometricPrompt
import android.os.CancellationSignal
import android.os.SystemClock
import android.util.Log
import android.view.ViewGroup
import android.view.WindowInsets
import androidx.activity.compose.BackHandler
import androidx.activity.compose.LocalActivity
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.compose.ui.window.DialogWindowProvider
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.compose.LocalLifecycleOwner
import kotlinx.coroutines.flow.first

/// The app lock. With it on, the app opens behind a cover that the
/// phone's own check lifts: a face or a fingerprint, or the screen lock's
/// PIN, pattern or password in their place. A fresh start of the app is
/// always locked; coming back to it is only once it was away for
/// `lockAfterSeconds` or longer. The cover is over the app, never instead
/// of it: the connections and screens under it carry on.
///
/// `init` once from the activity, and the app's screens inside
/// `AppLockGate`; the settings call `turnOn`, `turnOff` and `setLockAfter`.
object AppLock {
    /// How long the app may be away before coming back asks again, in
    /// seconds: 1, 5, 10 and 20 minutes, and an hour.
    val choices = listOf(60, 300, 600, 1200, 3600)
    const val DEFAULT_AFTER = 300

    private const val KEY_ON = "applock.on"
    private const val KEY_AFTER = "applock.after"

    private var prefs: SharedPreferences? = null

    var enabled by mutableStateOf(false)
        private set
    var lockAfterSeconds by mutableIntStateOf(DEFAULT_AFTER)
        private set
    /// The cover is up.
    var locked by mutableStateOf(false)
        private set

    /// Counts the covers put up, so each asks by itself once.
    internal var lockCount by mutableIntStateOf(0)
        private set

    /// The last cover asked for by itself.
    internal var askedFor = 0

    /// The lock's own screens (`LockActivity`) made and not yet gone, by
    /// the task they are in: each of the app's tasks needs its own.
    internal val screens = mutableStateMapOf<Int, Int>()

    /// The tasks one was asked for in and is not made yet.
    internal val opening = mutableStateMapOf<Int, Boolean>()

    /// The lock's own screen is up in this task, or on its way.
    internal fun screenUp(task: Int): Boolean = (screens[task] ?: 0) > 0 || opening.containsKey(task)

    /// When the app left the screen, unlocked, on the clock that runs on
    /// while the phone sleeps and that changing the phone's time does not
    /// move.
    private var awayAt: Long? = null

    /// The check on screen now, so a second is not asked over it.
    private var asking: CancellationSignal? = null

    /// What another app handed back while the cover was up (a file picked,
    /// say), kept for when it lifts.
    private val held = mutableListOf<() -> Unit>()

    /// Reads the setting; on, the app starts locked. Once per process: a
    /// screen made again keeps the lock as it was.
    fun init(context: Context) {
        if (prefs != null) return
        val p = context.applicationContext.getSharedPreferences("thinkterm-applock", Context.MODE_PRIVATE)
        prefs = p
        enabled = p.getBoolean(KEY_ON, false)
        lockAfterSeconds = p.getInt(KEY_AFTER, DEFAULT_AFTER).takeIf { it in choices } ?: DEFAULT_AFTER
        if (enabled) lock()
    }

    /// The phone has a screen lock, which any face or fingerprint is set
    /// up under: without one there is nothing to check with.
    fun available(context: Context): Boolean =
        context.getSystemService(KeyguardManager::class.java)?.isDeviceSecure == true

    /// The phone says outright, two ways, that it has no screen lock. Not
    /// the same as `available` false, which a system service that did not
    /// answer says too: that keeps the cover up.
    private fun noScreenLock(context: Context): Boolean {
        if (available(context)) return false
        val biometrics = context.getSystemService(BiometricManager::class.java) ?: return false
        return biometrics.canAuthenticate(Authenticators.BIOMETRIC_WEAK or Authenticators.DEVICE_CREDENTIAL) ==
            BiometricManager.BIOMETRIC_ERROR_NONE_ENROLLED
    }

    /// Turns the lock on once the check passes here, so a lock that could
    /// not be lifted is never set. `done` hears whether it is on.
    fun turnOn(activity: Activity, title: String, done: (Boolean) -> Unit = {}) {
        if (enabled) return done(true)
        if (!available(activity)) return done(false)
        ask(activity, title) { ok ->
            if (ok) {
                enabled = true
                awayAt = null
                prefs?.edit()?.putBoolean(KEY_ON, true)?.apply()
            }
            done(ok)
        }
    }

    /// Turns the lock off once the check passes, as it went on: else
    /// anyone holding the app while it is open (back within the time set)
    /// could take the lock off for good. A phone with no screen lock left
    /// has nothing to check with, and the lock lets anyone in already: off
    /// at once. `done` hears whether it is off.
    fun turnOff(activity: Activity, title: String, done: (Boolean) -> Unit = {}) {
        if (!enabled) return done(true)
        if (noScreenLock(activity)) {
            off()
            return done(true)
        }
        ask(activity, title) { ok ->
            if (ok || noScreenLock(activity)) off()
            done(!enabled)
        }
    }

    private fun off() {
        enabled = false
        awayAt = null
        prefs?.edit()?.putBoolean(KEY_ON, false)?.apply()
        lift()
    }

    /// While the cover is up, keeps `deliver` for when it lifts and says
    /// so; the app hands it what comes back from another app (the
    /// activity's `onActivityResult`), so nothing picked there is acted on
    /// before the check passes. False, and nothing kept, when unlocked.
    fun holdWhileLocked(deliver: () -> Unit): Boolean {
        // It arrives before the app is back in front: away too long in the
        // other app, it is locked first.
        expireIfAway()
        if (!locked) return false
        held += deliver
        return true
    }

    fun setLockAfter(seconds: Int) {
        if (seconds !in choices) return
        lockAfterSeconds = seconds
        prefs?.edit()?.putInt(KEY_AFTER, seconds)?.apply()
    }

    /// The app's screen paused or stopped (another app over it, a picker
    /// that leaves it in sight, or the background): the first such moment
    /// counts. The lock's own screen and checks come while locked or while
    /// turning it on, and do not.
    internal fun wentAway() {
        if (enabled && !locked && awayAt == null) awayAt = SystemClock.elapsedRealtime()
    }

    /// In front again and taking input: the time away ends here. Seen
    /// again but with another app's picker still over it, it has not.
    internal fun cameBack() {
        expireIfAway()
        awayAt = null
    }

    /// Locks if away too long by now, the time away running on.
    internal fun expireIfAway() {
        val at = awayAt ?: return
        if (enabled && !locked && SystemClock.elapsedRealtime() - at >= lockAfterSeconds * 1000L) {
            awayAt = null
            lock()
        }
    }

    private fun lock() {
        locked = true
        lockCount++
    }

    private fun lift() {
        locked = false
        val kept = held.toList()
        held.clear()
        kept.forEach { it() }
    }

    /// Asks, and lifts the cover when the check passes. A phone whose
    /// screen lock was taken off since has nothing to check with, and
    /// anyone may open it: the cover lifts with no question.
    internal fun unlock(activity: Activity, title: String) {
        if (!locked) return
        if (noScreenLock(activity)) return lift()
        ask(activity, title) { ok -> if (ok || noScreenLock(activity)) lift() }
    }

    /// The system's check: a face or a fingerprint, and the screen lock
    /// itself for one that is not set up or not recognised.
    private fun ask(activity: Activity, title: String, done: (Boolean) -> Unit) {
        if (asking != null) return
        val signal = CancellationSignal()
        asking = signal
        val callback = object : BiometricPrompt.AuthenticationCallback() {
            // A check cancelled since (its activity gone) is not heard.
            override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                if (asking !== signal) return
                asking = null
                done(true)
            }

            // Cancelled, timed out, or nothing to check with; a face or a
            // finger not recognised is not an end, the check stays up.
            override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                if (asking !== signal) return
                asking = null
                done(false)
            }
        }
        try {
            BiometricPrompt.Builder(activity)
                .setTitle(title)
                .setAllowedAuthenticators(Authenticators.BIOMETRIC_WEAK or Authenticators.DEVICE_CREDENTIAL)
                .build()
                .authenticate(signal, activity.mainExecutor, callback)
        } catch (e: RuntimeException) {
            Log.w("thinkterm", "app lock: the check did not start ($e)")
            asking = null
            done(false)
        }
    }

    /// The activity that asked is going: its check goes with it.
    internal fun cancelAsk() {
        asking?.cancel()
        asking = null
    }

}

/// The words the cover shows, in the app's language.
class AppLockTexts(
    /// Over the cover: "ThinkTerm is locked".
    val locked: String,
    /// Its button.
    val unlock: String,
    /// The title of the system's check.
    val prompt: String,
)

/// The app inside the lock. While locked, the cover is over it three
/// times: in its own place; in a dialog of its own, over every dialog the
/// app has open, for the moment before the lock's screen is up; and as
/// that screen (`LockActivity`), above the app's, which every dialog the
/// app opens, now or later, stays under. That screen asks by itself, once
/// per lock.
@Composable
fun AppLockGate(texts: AppLockTexts, content: @Composable () -> Unit) {
    val activity = LocalActivity.current
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val words by rememberUpdatedState(texts)
    DisposableEffect(lifecycle, activity) {
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_PAUSE, Lifecycle.Event.ON_STOP -> AppLock.wentAway()
                Lifecycle.Event.ON_START, Lifecycle.Event.ON_RESUME -> {
                    if (event == Lifecycle.Event.ON_RESUME) AppLock.cameBack() else AppLock.expireIfAway()
                    // Back after too long: the lock's screen goes up now,
                    // before the app's own windows are seen again.
                    if (activity != null && AppLock.locked && !AppLock.screenUp(activity.taskId)) LockActivity.show(activity, words)
                }
                else -> {}
            }
        }
        lifecycle.addObserver(observer)
        onDispose {
            lifecycle.removeObserver(observer)
            AppLock.cancelAsk()
        }
    }
    // With the lock on, the system's list of recent apps shows a blank
    // card for the app, not its last screen. Screenshots are not stopped.
    val enabled = AppLock.enabled
    LaunchedEffect(enabled, activity) { activity?.setRecentsScreenshotEnabled(!enabled) }

    val locked = AppLock.locked
    val view = LocalView.current
    val count = AppLock.lockCount
    val up = activity != null && AppLock.screenUp(activity.taskId)
    LaunchedEffect(locked, count, up) {
        if (!locked) return@LaunchedEffect
        // What is typed must not reach a terminal under the cover.
        view.rootView.findFocus()?.clearFocus()
        view.windowInsetsController?.hide(WindowInsets.Type.ime())
        if (up) return@LaunchedEffect
        val at = activity ?: return@LaunchedEffect
        lifecycle.currentStateFlow.first { it.isAtLeast(Lifecycle.State.STARTED) }
        if (AppLock.locked && !AppLock.screenUp(at.taskId)) LockActivity.show(at, texts)
    }

    Box(Modifier.fillMaxSize()) {
        // Hidden from the screen reader too while covered.
        Box(if (locked) Modifier.fillMaxSize().clearAndSetSemantics {} else Modifier.fillMaxSize()) { content() }
        if (locked) {
            BackHandler { activity?.moveTaskToBack(true) }
            LockScreen(texts) { activity?.let { AppLock.unlock(it, texts.prompt) } }
        }
    }
    if (locked) {
        Dialog(
            // Back leaves the app as it is, locked.
            onDismissRequest = { activity?.moveTaskToBack(true) },
            properties = DialogProperties(
                dismissOnClickOutside = false,
                usePlatformDefaultWidth = false,
                decorFitsSystemWindows = false,
            ),
        ) {
            // The whole screen, at once and undimmed: it lands exactly on
            // the cover under it, so nothing is seen to move.
            val window = (LocalView.current.parent as? DialogWindowProvider)?.window
            SideEffect {
                window?.setLayout(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT)
                window?.setDimAmount(0f)
                window?.setWindowAnimations(0)
            }
            LockScreen(texts) { activity?.let { AppLock.unlock(it, texts.prompt) } }
        }
    }
}
