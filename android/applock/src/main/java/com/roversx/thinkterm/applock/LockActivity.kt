package com.roversx.thinkterm.applock

import android.app.Activity
import android.app.ActivityOptions
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Build
import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.flow.first

/// The lock's own screen, over the app's. A screen of its own and not a
/// dialog in the app's, because a dialog the app opens later would come
/// up over a dialog, never over another screen. See-through as far as the
/// system goes (its own cover is opaque), so the app's screen under it is
/// only paused, not stopped: the app does not take it for leaving.
class LockActivity : ComponentActivity() {
    companion object {
        private const val LOCKED = "locked"
        private const val UNLOCK = "unlock"
        private const val PROMPT = "prompt"

        /// Over `from`, at once, with no animation: it lands exactly on the
        /// cover already over the app.
        internal fun show(from: Activity, texts: AppLockTexts) {
            val intent = Intent(from, LockActivity::class.java)
                .putExtra(LOCKED, texts.locked)
                .putExtra(UNLOCK, texts.unlock)
                .putExtra(PROMPT, texts.prompt)
            AppLock.opening[from.taskId] = true
            try {
                from.startActivity(intent, ActivityOptions.makeCustomAnimation(from, 0, 0).toBundle())
            } catch (e: RuntimeException) {
                Log.w("thinkterm", "app lock: its screen did not open ($e)")
                AppLock.opening.remove(from.taskId)
            }
        }
    }

    private lateinit var texts: AppLockTexts

    /// The task this screen covers, as counted in `AppLock.screens`.
    private var task = -1

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            overrideActivityTransition(OVERRIDE_TRANSITION_OPEN, 0, 0)
            overrideActivityTransition(OVERRIDE_TRANSITION_CLOSE, 0, 0)
        }
        // Brought back by the system after the app's process ended: the
        // lock is read again, and is on with the setting.
        AppLock.init(this)
        task = taskId
        AppLock.opening.remove(task)
        AppLock.screens[task] = (AppLock.screens[task] ?: 0) + 1
        setRecentsScreenshotEnabled(false)
        if (!AppLock.locked) return leave()
        texts = AppLockTexts(
            locked = intent.getStringExtra(LOCKED) ?: "",
            unlock = intent.getStringExtra(UNLOCK) ?: "",
            prompt = intent.getStringExtra(PROMPT) ?: "",
        )
        // Back leaves the app as it is, locked.
        onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() {
                moveTaskToBack(true)
            }
        })
        setContent {
            LaunchedEffect(Unit) {
                snapshotFlow { AppLock.locked }.first { !it }
                leave()
            }
            LockScreen(texts) { AppLock.unlock(this, texts.prompt) }
        }
    }

    override fun onResume() {
        super.onResume()
        // Asks by itself, once per lock: cancelled, the button asks again.
        if (::texts.isInitialized && AppLock.locked && AppLock.askedFor != AppLock.lockCount) {
            AppLock.askedFor = AppLock.lockCount
            window.decorView.post { AppLock.unlock(this, texts.prompt) }
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        val left = (AppLock.screens[task] ?: 1) - 1
        if (left > 0) AppLock.screens[task] = left else AppLock.screens.remove(task)
        if (!isChangingConfigurations) AppLock.cancelAsk()
    }

    private fun leave() {
        finish()
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            @Suppress("DEPRECATION")
            overridePendingTransition(0, 0)
        }
    }
}

/// The cover: the app's icon, the words, and the button that asks again,
/// in colours of its own, so it is the same over the app and on its own
/// screen. Touches anywhere but the button go no further.
@Composable
internal fun LockScreen(texts: AppLockTexts, onUnlock: () -> Unit) {
    val dark = isSystemInDarkTheme()
    val background = if (dark) Color.Black else Color.White
    val ink = if (dark) Color.White else Color.Black
    val scheme = (if (dark) darkColorScheme() else lightColorScheme()).copy(
        background = background,
        onBackground = ink,
        primary = ink,
        onPrimary = background,
    )
    MaterialTheme(colorScheme = scheme) {
        Box(Modifier.fillMaxSize().background(background), contentAlignment = Alignment.Center) {
            // Under the words and the button, so a press on the button is
            // its own and a touch elsewhere stops here.
            Box(
                Modifier.matchParentSize().pointerInput(Unit) {
                    awaitPointerEventScope {
                        while (true) awaitPointerEvent().changes.forEach { it.consume() }
                    }
                },
            )
            Column(Modifier.padding(horizontal = 32.dp), horizontalAlignment = Alignment.CenterHorizontally) {
                AppIcon()
                Spacer(Modifier.height(18.dp))
                Text(texts.locked, style = MaterialTheme.typography.titleMedium, color = ink, textAlign = TextAlign.Center)
                Spacer(Modifier.height(24.dp))
                Button(onClick = onUnlock) { Text(texts.unlock) }
            }
        }
    }
}

/// The app's own launcher icon, drawn once the cover is up.
@Composable
private fun AppIcon() {
    val context = LocalContext.current
    val px = with(LocalDensity.current) { 84.dp.roundToPx() }
    val icon = remember(context, px) { appIcon(context, px) } ?: return
    Image(icon, contentDescription = null, modifier = Modifier.size(84.dp))
}

private fun appIcon(context: Context, px: Int): ImageBitmap? = runCatching {
    val drawable = context.packageManager.getApplicationIcon(context.applicationInfo)
    val bitmap = Bitmap.createBitmap(px, px, Bitmap.Config.ARGB_8888)
    drawable.setBounds(0, 0, px, px)
    drawable.draw(Canvas(bitmap))
    bitmap.asImageBitmap()
}.getOrNull()
