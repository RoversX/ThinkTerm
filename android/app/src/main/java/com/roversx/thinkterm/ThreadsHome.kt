package com.roversx.thinkterm

import android.os.Build
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.expandHorizontally
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkHorizontally
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import com.roversx.thinkterm.core.RepoProject
import com.roversx.thinkterm.core.repoGroups
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.kyant.backdrop.backdrops.layerBackdrop
import com.kyant.backdrop.backdrops.rememberLayerBackdrop
import kotlinx.coroutines.delay

/// The ground of the app's own lists -- the threads, the sidebar -- in the
/// system's lightness, as iOS's `chromeBackdrop`.
fun listBackdrop(dark: Boolean) = if (dark) Color(0xFF121212) else Color(0xFFF2F2F2)

/// The widest a list runs on a wide screen: wider, it keeps to the middle.
val READABLE_WIDTH = 720.dp

/// The blue of the one button that leads (New Thread), in either lightness.
val LeadBlue = Color(0xFF0A84FF)

/// Where the app opens: the threads of one Space, of any of the hosts --
/// the Space on show picked at the list's head, as the desktop's Space
/// menu picks it, or swiped to -- the same list a terminal's sidebar is.
/// A thread tapped opens its host's terminal on it; a host this phone has
/// been in before is connected to as the screen comes up, and one it has
/// not waits for a tap. From ios/Sources/ThreadsHome.swift.
@Composable
fun ThreadsHome(
    store: HostStore,
    onOpen: (Host) -> Unit,
    onHosts: () -> Unit,
    onSettings: () -> Unit,
) {
    val context = LocalContext.current
    val entries = listedHosts(context, store)
    // Populate threads on arrival for known hosts that allow automatic connections.
    LaunchedEffect(Unit) {
        for (host in entries) {
            if (Sessions.existing(host.id) != null) continue
            if (!host.autoConnect || (host.id != Host.PROBE_ID && host.lastConnected == null)) continue
            Sessions.get(context, store, host).connect()
        }
    }
    val dark = isSystemInDarkTheme()
    CompositionLocalProvider(LocalChrome provides Chrome(dark = dark)) {
        SpaceList(
            place = ListPlace.HOME,
            store = store,
            host = shownHost(entries),
            onOpen = onOpen,
            onSettings = onSettings,
            onHosts = onHosts,
        )
    }
}

/// A row as the desktop's sidebar draws it: an icon (or a thread's cards),
/// the title, and at the end a thread's mark or a button of its own.
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun HomeRow(
    icon: String? = null,
    lead: (@Composable () -> Unit)? = null,
    mark: (@Composable () -> Unit)? = null,
    title: String,
    subtitle: String? = null,
    depth: Int = 0,
    bold: Boolean = false,
    dim: Boolean = false,
    selected: Boolean = false,
    /// Unfolded (true), folded (false), or nothing to fold (null).
    chevron: Boolean? = null,
    trailing: (@Composable () -> Unit)? = null,
    /// More under the subtitle, in the text's column: an agent's progress
    /// and sub-tasks. The row then grows down from its first line, which
    /// keeps the lead and the end in line with it.
    below: (@Composable () -> Unit)? = null,
    onLongClick: (() -> Unit)? = null,
    onClick: () -> Unit,
) {
    val primary = MaterialTheme.colorScheme.onSurface
    val secondary = MaterialTheme.colorScheme.onSurfaceVariant
    val rowGlass = LocalRowGlass.current
    // With something below, each piece is centred on the first line's
    // height rather than on the whole row's.
    val line = if (below != null) Modifier.heightIn(min = 44.dp) else Modifier
    val grows = below != null
    Row(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 44.dp)
            // The row on show: the desktop's pill, in glass where the list
            // has a layer behind it. Drawn, not clipped to: a thread's cards
            // cast their shadows past the row's edges.
            .then(
                when {
                    !selected -> Modifier
                    rowGlass != null -> Modifier.glass(rowGlass, MaterialTheme.colorScheme.surface.luminance() < 0.5f, CircleShape)
                    else -> Modifier
                        .background(primary.copy(alpha = 0.13f), CircleShape)
                        .border(1.dp, primary.copy(alpha = 0.1f), CircleShape)
                }
            )
            .combinedClickable(onClick = onClick, onLongClick = onLongClick)
            .padding(start = (12 + depth * 18).dp, end = 10.dp),
        verticalAlignment = if (grows) Alignment.Top else Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        when {
            lead != null -> FirstLine(grows) { lead() }
            icon != null -> Box(line.width(24.dp), contentAlignment = Alignment.Center) {
                DesktopIcon(icon, tint = secondary)
            }
        }
        Column(Modifier.weight(1f)) {
            Column(line, verticalArrangement = Arrangement.Center) {
                Text(
                    title,
                    fontSize = 16.sp,
                    fontWeight = if (bold || lead != null) FontWeight.SemiBold else FontWeight.Normal,
                    color = if (dim) secondary else primary,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (subtitle != null) {
                    Text(subtitle, fontSize = 13.sp, color = secondary, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }
            if (below != null) below()
        }
        if (trailing != null) FirstLine(grows) {
            CompositionLocalProvider(LocalContentColor provides secondary) { trailing() }
        }
        if (mark != null) FirstLine(grows) { Box(Modifier.padding(end = 4.dp)) { mark() } }
        if (chevron != null) FirstLine(grows) {
            DesktopIcon("chevron-right", size = 14.dp, tint = secondary, modifier = Modifier.rotate(if (chevron) 90f else 0f))
        }
    }
}

/// `content` centred on a row's first line when the row grows below it
/// (`HomeRow`'s `below`); left as it is otherwise.
@Composable
private fun FirstLine(grows: Boolean, content: @Composable () -> Unit) {
    if (grows) Box(Modifier.heightIn(min = 44.dp), contentAlignment = Alignment.CenterStart) { content() } else content()
}
