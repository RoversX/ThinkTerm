package com.roversx.thinkterm

import android.app.Activity
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.ui.unit.dp
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.ReadOnlyComposable
import androidx.compose.runtime.SideEffect
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.platform.LocalView
import androidx.core.view.WindowCompat

/// The colours of the chrome around a terminal. The chrome takes the
/// terminal's own background, so its ink follows the scheme, not the
/// system: light on a dark scheme, dark on a light one, as on iOS.
@Immutable
data class Chrome(val dark: Boolean) {
    val ink: Color get() = if (dark) Color.White else Color(0xFF1C1C1E)
    /// A bar's unfocused text.
    val dim: Color get() = if (dark) Color(0xFF9E9E9E) else Color(0xFF6E6E73)
    /// The copy chip and the status line.
    val raised: Color get() = if (dark) Color(0xFF2C2C2E) else Color(0xFFE5E5EA)
    /// The cards over the terminal.
    val card: Color get() = if (dark) Color(0xF0202124) else Color(0xF0F2F2F7)

    companion object {
        fun of(background: Color) = Chrome(dark = background.luminance() < 0.5f)
    }
}

val LocalChrome = staticCompositionLocalOf { Chrome(dark = true) }

/// The chrome's ink, where a composable used to write white.
val ink: Color
    @Composable @ReadOnlyComposable
    get() = LocalChrome.current.ink

/// The colours Material's remaining pieces (menus, dialogs, spinners) draw
/// with: iOS's grounds and the desktop's blue, not Material's own purple.
fun appColors(dark: Boolean) = if (dark) {
    darkColorScheme(
        primary = LeadBlue,
        onPrimary = Color.White,
        secondary = LeadBlue,
        background = Color.Black,
        surface = Color(0xFF1C1C1E),
        surfaceContainer = Color(0xFF2C2C2E),
        surfaceContainerHigh = Color(0xFF2C2C2E),
        surfaceContainerHighest = Color(0xFF3A3A3C),
        onSurface = Color.White,
        onSurfaceVariant = Color(0x99EBEBF5),
        outline = Color(0xFF38383A),
        outlineVariant = Color(0xFF38383A),
        error = Color(0xFFFF453A),
    )
} else {
    lightColorScheme(
        primary = LeadBlue,
        onPrimary = Color.White,
        secondary = LeadBlue,
        background = Color(0xFFF2F2F7),
        surface = Color.White,
        surfaceContainer = Color(0xFFF9F9F9),
        surfaceContainerHigh = Color(0xFFF9F9F9),
        surfaceContainerHighest = Color(0xFFEFEFF4),
        onSurface = Color.Black,
        onSurfaceVariant = Color(0x993C3C43),
        outline = Color(0xFFC6C6C8),
        outlineVariant = Color(0xFFC6C6C8),
        error = Color(0xFFFF3B30),
    )
}

/// Menus and dialogs rounded as iOS's are.
val appShapes = Shapes(
    extraSmall = RoundedCornerShape(14.dp),
    small = RoundedCornerShape(12.dp),
    medium = RoundedCornerShape(14.dp),
    large = RoundedCornerShape(16.dp),
    extraLarge = RoundedCornerShape(22.dp),
)

/// The terminal's screen and what sits on it: Material's own pieces
/// (menus, dialogs, the sidebar) in the scheme's lightness.
@Composable
fun TerminalChrome(background: Color, content: @Composable () -> Unit) {
    val chrome = Chrome.of(background)
    MaterialTheme(colorScheme = appColors(chrome.dark), shapes = appShapes) {
        CompositionLocalProvider(LocalChrome provides chrome, content = content)
    }
}

/// Light icons in the status and navigation bars over a dark screen,
/// dark ones over a light one -- and over the starry sky while it covers
/// the screen (`LaunchSky.covering`), unless one of the app's own pages is
/// up over it (`LaunchSky.paged`). One call per screen: two would take
/// turns as each recomposed.
@Composable
fun SystemBars(dark: Boolean) {
    val view = LocalView.current
    if (view.isInEditMode) return
    val overDark = dark || (LaunchSky.covering && !LaunchSky.paged)
    SideEffect {
        val window = (view.context as? Activity)?.window ?: return@SideEffect
        WindowCompat.getInsetsController(window, view).apply {
            isAppearanceLightStatusBars = !overDark
            isAppearanceLightNavigationBars = !overDark
        }
    }
}
